use crate::config::Config;
use crate::grove::GroveCli;
use crate::host::{self, Host, HostCapabilities, HostInfo};
use crate::{lifecycle, order};
use anyhow::{Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

#[derive(Serialize)]
struct Report {
    host: HostReport,
    /// Present when host.kind is grove (Grove capability pin details).
    #[serde(skip_serializing_if = "Option::is_none")]
    grove: Option<GroveDetail>,
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
struct HostReport {
    kind: String,
    version: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<HostCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<String>,
}

#[derive(Serialize)]
struct GroveDetail {
    bin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    capabilities: Option<crate::grove::Capabilities>,
}

#[derive(Serialize)]
struct Repo {
    git_repo: bool,
    git_identity: bool,
    ok: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
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
    let repo_path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (host, grove) = host_report(config, &repo_path, &mut notes, &mut next);
    let repo = repo(&mut next);
    let (orders, roles) = orders(config, loaded, &host.kind, &mut notes)?;
    let default_executor = config.default_executor();
    if !has_orders && default_executor.is_none() {
        next.push(
            "select a model with `summoner setup` (wizard) or `summoner setup --preset <codex|claude|kimi>` (or name an executor in every order)"
                .to_string(),
        );
    }
    let skill_paths = crate::skills::installed_paths();
    if skill_paths.is_empty() {
        notes.push(
            "no harness skill installed (Claude /summoner, Codex, Agents, Grok); run `summoner setup`"
                .into(),
        );
        next.push("summoner setup   # skills + model wizard (session or permanent)".into());
    } else {
        notes.push(format!(
            "harness skills: {}",
            skill_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
    let ok = host.ok && repo.ok && orders_ok && default_ok && roles_ok;
    Ok(Report {
        host,
        grove,
        repo,
        orders,
        default_executor,
        executors,
        notes,
        next_steps: next,
        ok,
    })
}

fn orders(
    config: &Config,
    loaded: Option<&[order::Order]>,
    host_kind: &str,
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
    let verification = verification(loaded, config, host_kind, notes, &mut problems);
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
    host_kind: &str,
    notes: &mut Vec<String>,
    problems: &mut Vec<String>,
) -> Vec<Verification> {
    let git_profiles = config
        .verification
        .as_ref()
        .map(|v| v.profiles.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();
    let grove = crate::config::grove_profiles(&std::env::current_dir().unwrap_or_default())
        .unwrap_or(crate::config::GroveProfiles {
            path: None,
            names: BTreeSet::new(),
            selected: None,
        });

    loaded
        .iter()
        .map(|item| {
            let profile = item
                .verify_profile
                .clone()
                .or_else(|| config.default_verify_profile.clone());
            let (configured, source) = match profile.as_ref() {
                None => (None, None),
                Some(name) if host_kind == "git" => {
                    let ok = git_profiles.contains(name);
                    if !ok {
                        // Vacuous pass when no profiles defined and name is unused — still warn.
                        if git_profiles.is_empty() {
                            problems.push(format!(
                                "{}: verification profile {name:?} is not defined under [verification.profiles]; git host will not invent a green check",
                                item.source.display()
                            ));
                            (Some(false), Some("[verification] in summoner config".into()))
                        } else {
                            problems.push(format!(
                                "{}: verification profile {name:?} is not defined under [verification.profiles] in summoner config",
                                item.source.display()
                            ));
                            (Some(false), Some("[verification] in summoner config".into()))
                        }
                    } else {
                        (Some(true), Some("[verification] in summoner config".into()))
                    }
                }
                Some(name) => {
                    let ok = grove.names.contains(name);
                    if !ok {
                        problems.push(format!(
                            "{}: verification profile {name:?} is not defined with a command in {}",
                            item.source.display(),
                            grove.path.as_ref().map_or_else(
                                || ".grove.toml".to_string(),
                                |path| path.display().to_string()
                            )
                        ));
                    }
                    (
                        Some(ok),
                        Some(
                            grove
                                .path
                                .as_ref()
                                .map_or_else(|| ".grove.toml".into(), |p| p.display().to_string()),
                        ),
                    )
                }
            };
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
                source,
            }
        })
        .collect()
}

fn host_report(
    config: &Config,
    repo: &std::path::Path,
    notes: &mut Vec<String>,
    next: &mut Vec<String>,
) -> (HostReport, Option<GroveDetail>) {
    let resolved = host::resolve(config, repo);
    if let Some(notice) = &resolved.notice {
        notes.push(notice.clone());
    }
    if resolved.kind == "git" {
        if repo.join(".grove.toml").is_file() {
            notes.push(
                "`.grove.toml` present: set [host] kind = \"grove\" to use CoW lanes, claims, and receipt-bound finish for multi-agent Rust"
                    .into(),
            );
        }
        return match host::open(config, repo).and_then(|h| h.preflight()) {
            Ok(info) => (host_from_info(info, resolved.notice), None),
            Err(error) => {
                next.push(format!("git host preflight failed: {error:#}"));
                (
                    HostReport {
                        kind: "git".into(),
                        version: String::new(),
                        ok: false,
                        state_root: None,
                        capabilities: None,
                        error: Some(format!("{error:#}")),
                        notice: resolved.notice,
                    },
                    None,
                )
            }
        };
    }

    let bin = resolved
        .grove_bin
        .clone()
        .unwrap_or_else(|| config.grove_bin());
    let cli = GroveCli::new(bin.clone());
    match cli.preflight() {
        Ok(capabilities) => {
            let version = cli.version().unwrap_or_else(|_| bin.clone());
            let info = HostInfo {
                kind: "grove".into(),
                version: version.clone(),
                state_root: None,
                capabilities: host::GroveHost::new(bin.clone()).capabilities(),
            };
            (
                host_from_info(info, resolved.notice),
                Some(GroveDetail {
                    bin,
                    capabilities: Some(capabilities),
                }),
            )
        }
        Err(error) => {
            next.push(
                "install a compatible Grove release and put `grove` on PATH, or set [host] kind = \"git\" for independence"
                    .into(),
            );
            (
                HostReport {
                    kind: "grove".into(),
                    version: String::new(),
                    ok: false,
                    state_root: None,
                    capabilities: None,
                    error: Some(format!("{error:#}")),
                    notice: resolved.notice,
                },
                Some(GroveDetail {
                    bin,
                    capabilities: None,
                }),
            )
        }
    }
}

fn host_from_info(info: HostInfo, notice: Option<String>) -> HostReport {
    HostReport {
        kind: info.kind,
        version: info.version,
        ok: true,
        state_root: info.state_root.map(|p| p.display().to_string()),
        capabilities: Some(info.capabilities),
        error: None,
        notice,
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
