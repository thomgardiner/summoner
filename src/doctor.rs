use crate::config::Config;
use crate::grove::GroveCli;
use crate::{lifecycle, order};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize)]
struct Report {
    grove: Grove,
    repo: Repo,
    #[serde(skip_serializing_if = "Option::is_none")]
    orders: Option<Orders>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_executor: Option<String>,
    executors: Vec<lifecycle::Executor>,
    notes: Vec<String>,
    next_steps: Vec<String>,
    ok: bool,
}

#[derive(Serialize)]
struct Repo {
    git_repo: bool,
    git_identity: bool,
    ok: bool,
}

#[derive(Serialize)]
struct Grove {
    bin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<crate::grove::Capabilities>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Orders {
    count: usize,
    roles: Vec<String>,
    verification: Vec<Verification>,
    problems: Vec<String>,
    ok: bool,
}

#[derive(Serialize)]
struct Verification {
    order: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    configured: Option<bool>,
}

pub fn run(config: &Config, paths: &[PathBuf], allow_unknown_auth: bool) -> Result<i32> {
    let loaded = (!paths.is_empty())
        .then(|| order::load(paths))
        .transpose()?;
    let report = inspect(config, loaded.as_deref(), allow_unknown_auth)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if report.ok { 0 } else { 1 })
}

pub(crate) fn require(
    config: &Config,
    orders: &[order::Order],
    allow_unknown_auth: bool,
) -> Result<()> {
    let report = inspect(config, Some(orders), allow_unknown_auth)?;
    if report.ok {
        return Ok(());
    }
    bail!(
        "preflight failed before dispatch:\n{}",
        serde_json::to_string_pretty(&report)?
    )
}

fn inspect(
    config: &Config,
    loaded: Option<&[order::Order]>,
    allow_unknown_auth: bool,
) -> Result<Report> {
    let has_orders = loaded.is_some();
    let mut notes = Vec::new();
    let mut next = Vec::new();
    let grove = grove(config, &mut next);
    let repo = repo(&mut next);
    let (orders, roles) = orders(config, loaded, &mut notes)?;
    let default_executor = config.default_executor();
    if !has_orders && default_executor.is_none() {
        next.push(
            "select a default with `summoner init --global --preset codex` (or name an executor in every order)"
                .to_string(),
        );
    }
    let executors = lifecycle::inspect(config, &roles, &mut next, allow_unknown_auth)?;
    let roles_ok = !roles.is_empty()
        && roles.iter().all(|name| config.executors.contains_key(name))
        && executors.iter().all(lifecycle::runnable);
    let orders_ok = orders.as_ref().is_none_or(|orders| orders.ok);
    let default_ok = has_orders
        || default_executor
            .as_ref()
            .is_some_and(|name| config.executors.contains_key(name));
    let ok = grove.ok && repo.ok && orders_ok && default_ok && roles_ok;
    let report = Report {
        grove,
        repo,
        orders,
        default_executor,
        executors,
        notes,
        next_steps: next,
        ok,
    };
    Ok(report)
}

fn orders(
    config: &Config,
    loaded: Option<&[order::Order]>,
    notes: &mut Vec<String>,
) -> Result<(Option<Orders>, BTreeSet<String>)> {
    let Some(loaded) = loaded else {
        let mut roles = BTreeSet::new();
        roles.extend(config.default_executor());
        roles.extend(config.default_reviewer());
        return Ok((None, roles));
    };
    for warning in order::warnings(loaded, config) {
        notes.push(format!("order warning: {warning}"));
    }
    let mut problems = order::validate(loaded, config);
    let roles = lifecycle::roles(loaded, config);
    let profiles = crate::config::grove_profiles(&std::env::current_dir()?)?;
    let verification = verification(loaded, config, &profiles, notes, &mut problems);
    let ok = problems.is_empty();
    Ok((
        Some(Orders {
            count: loaded.len(),
            roles: roles.iter().cloned().collect(),
            verification,
            problems,
            ok,
        }),
        roles,
    ))
}

fn verification(
    loaded: &[order::Order],
    config: &Config,
    profiles: &crate::config::GroveProfiles,
    notes: &mut Vec<String>,
    problems: &mut Vec<String>,
) -> Vec<Verification> {
    loaded.iter().map(|item| {
        let profile = item
            .verify_profile
            .clone()
            .or_else(|| config.default_verify_profile.clone());
        let configured = profile.as_ref().map(|name| profiles.names.contains(name));
        if let Some(name) = profile.as_ref()
            && configured == Some(false)
        {
            problems.push(format!(
                "{}: verification profile {name:?} is not defined with a command in {}",
                item.source.display(),
                profiles
                    .path
                    .as_ref()
                    .map_or_else(|| ".grove.toml".to_string(), |path| path.display().to_string())
            ));
        }
        if profile.is_none() {
            notes.push(format!(
                "order {:?} has no verification profile; a successful run stops at completed, not verified",
                item.id
            ));
        }
        Verification {
            order: item.id.clone(),
            profile,
            configured,
        }
    }).collect()
}

fn grove(config: &Config, next: &mut Vec<String>) -> Grove {
    let bin = config.grove_bin();
    let cli = GroveCli::new(bin.clone());
    match cli.preflight() {
        Ok(capabilities) => Grove {
            bin,
            capabilities: Some(capabilities),
            ok: true,
            error: None,
        },
        Err(error) => {
            next.push(
                "install a compatible current Grove release, ensure `grove` is on PATH, then rerun `summoner doctor`"
                    .to_string(),
            );
            Grove {
                bin,
                capabilities: None,
                ok: false,
                error: Some(format!("{error:#}")),
            }
        }
    }
}

fn repo(next: &mut Vec<String>) -> Repo {
    let git_repo = git_ok(&["rev-parse", "--git-dir"]);
    let git_identity =
        git_ok(&["config", "--get", "user.name"]) && git_ok(&["config", "--get", "user.email"]);
    if !git_repo {
        next.push(
            "run `git init` at the repository root, then rerun `summoner doctor`".to_string(),
        );
    } else if !git_identity {
        next.push(
            "set Git identity with `git config --global user.name \"Your Name\"` and `git config --global user.email \"you@example.com\"`"
                .to_string(),
        );
    }
    Repo {
        git_repo,
        git_identity,
        ok: git_repo && git_identity,
    }
}

fn git_ok(args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}
