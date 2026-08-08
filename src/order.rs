use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Order {
    pub id: String,
    pub title: String,
    pub brief: String,
    pub scope: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub executor: Option<String>,
    pub verify_profile: Option<String>,
    pub timeout_secs: Option<u64>,
    pub base: Option<String>,
    pub branch: Option<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(skip)]
    pub source: PathBuf,
}

pub fn load(paths: &[PathBuf]) -> Result<Vec<Order>> {
    if paths.is_empty() {
        bail!("no order files given");
    }
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(path)
                .with_context(|| format!("reading order directory {}", path.display()))?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|entry| {
                    entry.is_file()
                        && entry
                            .extension()
                            .is_some_and(|ext| ext == "toml" || ext == "json")
                })
                .collect();
            entries.sort();
            if entries.is_empty() {
                bail!(
                    "order directory {} contains no .toml or .json files",
                    path.display()
                );
            }
            files.extend(entries);
        } else {
            files.push(path.clone());
        }
    }
    files.into_iter().map(parse).collect()
}

fn parse(path: PathBuf) -> Result<Order> {
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading order {}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    let mut order: Order = match extension {
        "toml" => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        "json" => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        _ => bail!("{}: order must be .toml or .json", path.display()),
    };
    order.source = path.canonicalize().unwrap_or(path);
    Ok(order)
}

pub fn validate(orders: &[Order], config: &Config) -> Result<()> {
    let mut ids = BTreeSet::new();
    for order in orders {
        let at = order.source.display();
        if order.id.is_empty()
            || !order
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            bail!("{at}: id {:?} must match [a-z0-9_-]+", order.id);
        }
        if !ids.insert(order.id.clone()) {
            bail!("{at}: duplicate order id {:?}", order.id);
        }
        if order.title.trim().is_empty() || order.brief.trim().is_empty() {
            bail!("{at}: title and brief must be non-empty");
        }
        if order.after.len() > 1 {
            bail!("{at}: after may contain zero or one parent");
        }
        if let Some(timeout) = order.timeout_secs
            && !(1..=604_800).contains(&timeout)
        {
            bail!("{at}: timeout_secs must be between 1 and 604800");
        }
        for scope in &order.scope {
            normalize_scope(scope).with_context(|| format!("{at}: invalid scope {scope:?}"))?;
        }
        if order.scope.is_empty() {
            bail!("{at}: scope must contain at least one path");
        }
        if let Some(branch) = order.branch.as_deref() {
            if branch.starts_with('-') {
                bail!("{at}: branch must not begin with '-'");
            }
            let valid = Command::new("git")
                .args(["check-ref-format", "--branch", branch])
                .output()
                .with_context(|| format!("{at}: validating branch {branch:?}"))?
                .status
                .success();
            if !valid {
                bail!("{at}: invalid branch {branch:?}");
            }
        }
        let executor = order
            .executor
            .as_deref()
            .or(config.default_executor.as_deref())
            .ok_or_else(|| anyhow::anyhow!("{at}: no executor configured"))?;
        let backend = config
            .executors
            .get(executor)
            .ok_or_else(|| anyhow::anyhow!("{at}: executor {executor:?} is not configured"))?;
        if backend.argv.is_empty() {
            bail!("{at}: executor {executor:?} has empty argv");
        }
        let profile = order
            .verify_profile
            .as_deref()
            .or(config.default_verify_profile.as_deref());
        if let Some(name) = profile {
            let profile = config.verification.profiles.get(name).ok_or_else(|| {
                anyhow::anyhow!("{at}: verification profile {name:?} is not configured")
            })?;
            if profile.commands.is_empty() {
                bail!("{at}: verification profile {name:?} has no commands");
            }
        }
    }
    for order in orders {
        if let Some(parent) = order.after.first() {
            if parent == &order.id {
                bail!("{}: order cannot depend on itself", order.source.display());
            }
            if !ids.contains(parent) {
                bail!("{}: unknown parent {:?}", order.source.display(), parent);
            }
        }
    }
    if !topological_waves(orders).is_some() {
        bail!("dependency cycle detected");
    }
    Ok(())
}

pub fn topological_waves(orders: &[Order]) -> Option<Vec<Vec<String>>> {
    let mut pending: BTreeMap<&str, Option<&str>> = orders
        .iter()
        .map(|order| (order.id.as_str(), order.after.first().map(String::as_str)))
        .collect();
    let mut waves = Vec::new();
    while !pending.is_empty() {
        let mut ready: Vec<&str> = pending
            .iter()
            .filter(|(_, parent)| parent.is_none_or(|parent| !pending.contains_key(parent)))
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            return None;
        }
        ready.sort_unstable();
        for id in &ready {
            pending.remove(id);
        }
        waves.push(ready.into_iter().map(String::from).collect());
    }
    Some(waves)
}

pub fn normalize_scope(value: &str) -> Result<String> {
    let value = value.replace('\\', "/");
    if value.trim().is_empty() || value.starts_with('/') || value.contains(':') {
        bail!("scope must be a relative path");
    }
    let mut parts = Vec::new();
    for component in Path::new(&value).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::ParentDir => bail!("scope may not traverse a parent directory"),
            Component::RootDir | Component::Prefix(_) => bail!("scope must be relative"),
        }
    }
    if parts.is_empty() {
        Ok(".".into())
    } else {
        Ok(parts.join("/"))
    }
}

pub fn in_scope(path: &str, scopes: &[String]) -> bool {
    let path = path.replace('\\', "/");
    scopes.iter().any(|scope| {
        scope == "."
            || path == scope.as_str()
            || path
                .strip_prefix(scope)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

pub fn normalized_scopes(order: &Order) -> Result<Vec<String>> {
    order
        .scope
        .iter()
        .map(|scope| normalize_scope(scope))
        .collect()
}
