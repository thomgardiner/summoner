//! Work orders: one file per delegated task, TOML or JSON by extension. The
//! orchestrator writes them; summoner never decomposes plans itself.

use crate::config::Config;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

mod backend;
mod graph;
mod policy;
mod validate;
mod warnings;

pub(crate) use graph::depends_on;
pub use validate::validate;
pub use warnings::warnings;

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Order {
    pub id: String,
    pub title: String,
    pub brief: String,
    /// Passed verbatim to `grove task begin --scope`; grove resolves
    /// `crate:<name>` entries itself, so `grove plan --json` claim_scopes paste in.
    pub scope: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub verify_profile: Option<String>,
    pub executor: Option<String>,
    /// Executor name spawned as an independent reviewer after this order
    /// verifies; overrides `default_reviewer`. `"none"` opts this order out.
    pub reviewer: Option<String>,
    pub timeout_secs: Option<u64>,
    /// Token ceiling for this order. Usage is scraped after each executor
    /// exit, so this cannot stop a live overrun — but an over-budget attempt
    /// is never revised, and the overage is called out in the report.
    pub max_tokens: Option<u64>,
    pub base: Option<String>,
    pub branch: Option<String>,
    /// Order ids that must finish first. This orders dispatch AND supplies the
    /// base: a dependent branches from its dependencies' verified commits, so
    /// their code is present without naming a base by hand. One dependency is
    /// inherited directly, several are merged, and dependencies that conflict
    /// skip the order rather than starting it on a tree missing half its
    /// inputs. An explicit `base` still wins.
    #[serde(default)]
    pub after: Vec<String>,
    /// N-version dispatch: executor names that each attempt this order
    /// independently. The order expands into one sibling per executor
    /// (`<id>-<executor>`), all sharing a grove claim group so they may
    /// overlap the same scope; the orchestrator reviews and lands one winner.
    #[serde(default)]
    pub variants: Vec<String>,
    /// Internal: the grove claim group variant siblings share.
    #[serde(skip)]
    pub claim_group: Option<String>,
    /// Internal: the original order id a variant sibling was expanded from.
    #[serde(skip)]
    pub variant_of: Option<String>,
    #[serde(skip)]
    pub source: PathBuf,
}

impl Order {
    /// The grove agent identity, which also fixes the default branch
    /// (`grove/smn-<id>`). The prefix is what `summoner status` filters on.
    pub fn agent(&self) -> String {
        format!("smn-{}", self.id)
    }

    pub fn executor_name(&self, config: &Config) -> Option<String> {
        self.executor.clone().or_else(|| config.default_executor())
    }

    /// The reviewer to gate this order with, after config defaults and the
    /// `"none"` opt-out. None means the order ships ungated.
    pub fn reviewer_name(&self, config: &Config) -> Option<String> {
        match self.reviewer.as_deref() {
            Some("none") => None,
            Some(name) => Some(name.to_string()),
            None => config.default_reviewer(),
        }
    }
}

/// Load orders from files and directories. A directory contributes its
/// immediate `*.toml`/`*.json` entries sorted by name; nothing recursive.
pub fn load(paths: &[PathBuf]) -> Result<Vec<Order>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(path)
                .with_context(|| format!("reading order directory {}", path.display()))?
            {
                // A dropped entry would silently dispatch a subset of the batch.
                let entry = entry
                    .with_context(|| format!("reading an entry of {}", path.display()))?
                    .path();
                if entry.is_file()
                    && entry
                        .extension()
                        .is_some_and(|ext| ext == "toml" || ext == "json")
                {
                    entries.push(entry);
                }
            }
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
    if files.is_empty() {
        bail!("no order files given");
    }
    let orders: Vec<Order> = files
        .iter()
        .map(|path| parse(path))
        .collect::<Result<_>>()?;
    Ok(expand_variants(orders))
}

/// One sibling per variant executor, sharing a claim group so the deliberate
/// scope overlap does not conflict. Only one sibling's branch will land; the
/// orchestrator picks it during review.
fn expand_variants(orders: Vec<Order>) -> Vec<Order> {
    orders
        .into_iter()
        .flat_map(|order| {
            // An explicit executor alongside variants is ambiguous; leave it
            // unexpanded so validation names the problem.
            if order.variants.is_empty() || order.executor.is_some() {
                return vec![order];
            }
            order
                .variants
                .clone()
                .into_iter()
                .map(|executor| {
                    let mut sibling = order.clone();
                    sibling.id = format!("{}-{}", order.id, executor);
                    sibling.executor = Some(executor);
                    sibling.claim_group = Some(order.id.clone());
                    sibling.variant_of = Some(order.id.clone());
                    sibling.variants = Vec::new();
                    sibling
                })
                .collect()
        })
        .collect()
}

fn parse(path: &Path) -> Result<Order> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading order {}", path.display()))?;
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut order: Order = match extension {
        "toml" => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        "json" => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        other => bail!(
            "{}: unsupported order extension {other:?} (want .toml or .json)",
            path.display()
        ),
    };
    // Absolute: executors receive {order_file} while running from the leased
    // worktree, where a caller-relative path would point at nothing.
    order.source = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(order)
}

/// Only when the operator *opts in* to the git host. Auto-resolved git
/// (no grove present) still gets the check; explicit grove does not.
pub(crate) fn git_host_active(config: &Config) -> bool {
    if let Some(host) = &config.host
        && let Some(kind) = host.kind.as_deref()
    {
        return kind.eq_ignore_ascii_case("git");
    }
    // No explicit host: use resolver (git when no grove / no .grove.toml).
    let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::host::resolve(config, &repo).kind == "git"
}
