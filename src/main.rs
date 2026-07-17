//! summoner: fleet runner for LLM coding agents. The invoking session (any
//! harness) is the orchestrator; summoner deterministically dispatches
//! executor CLIs over grove-managed worktrees and reports back.
//!
//! Exit codes: 0 success (for `run`: every order verified), 1 domain outcome
//! needing review, 2 usage or infrastructure error.

mod config;
mod events;
mod executor;
mod grove;
mod init;
mod order;
mod plan;
mod report;
mod review;
mod run;
mod tripwires;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Fleet runner for LLM coding agents on grove worktrees.
#[derive(Parser)]
#[command(name = "summoner", version)]
struct Cli {
    /// Orchestrator profile from `[profiles.<name>]` in the config: picks the
    /// default executor and reviewer for whoever is invoking summoner. Also
    /// selectable via SUMMONER_PROFILE; auto-detected from harness
    /// environment markers when neither is given.
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Drop the orchestration contract: starter config, AGENTS.md section, Claude skill.
    Init {
        /// Instead write the executor template to ~/.config/summoner/config.toml,
        /// where personal executor definitions belong.
        #[arg(long)]
        global: bool,
    },
    /// Print the resolved configuration and where it came from.
    Config,
    /// Validate order files without dispatching anything.
    Check {
        /// Order files or directories of *.toml / *.json orders.
        paths: Vec<PathBuf>,
    },
    /// Analyze a proposed batch before dispatching: claim conflicts, package
    /// couplings, suggested waves, and after edges the orders do not declare.
    Plan {
        /// Order files or directories of *.toml / *.json orders.
        paths: Vec<PathBuf>,
    },
    /// Execute a fleet of work orders and print the ranked report.
    Run {
        /// Order files or directories of *.toml / *.json orders.
        paths: Vec<PathBuf>,
        /// Emit NDJSON lifecycle events on stdout as the fleet runs; the last
        /// line is a `report` event with the complete ranked report. The same
        /// events always land in events.jsonl in the run directory.
        #[arg(long)]
        stream: bool,
    },
    /// Re-run an earlier fleet: successful orders carry over, the rest
    /// dispatch again on their original branches.
    Resume {
        /// The run id from a previous report (also its run-directory name).
        run_id: String,
        /// Emit NDJSON lifecycle events, as with `run --stream`.
        #[arg(long)]
        stream: bool,
    },
    /// Summoner-owned grove tasks (owner prefix smn-), as JSON.
    Status,
    /// Check every configured executor and the grove binary; fail fast on setup problems.
    Doctor,
}

fn main() {
    let code = match dispatch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("summoner: {error:#}");
            2
        }
    };
    std::process::exit(code);
}

fn dispatch() -> Result<i32> {
    let cli = Cli::parse();
    let resolved = || -> Result<config::Resolved> {
        let mut resolved = config::load();
        if let Some(name) = config::select_profile(&mut resolved.config, cli.profile.as_deref())? {
            resolved.sources.push(format!("profile {name}"));
        }
        Ok(resolved)
    };
    match cli.cmd {
        Cmd::Init { global } => {
            let report = if global {
                init::init_global()?
            } else {
                init::init(&std::env::current_dir()?)?
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        Cmd::Config => {
            println!("{}", serde_json::to_string_pretty(&resolved()?)?);
            Ok(0)
        }
        Cmd::Check { paths } => {
            let resolved = resolved()?;
            let orders = order::load(&paths)?;
            for warning in order::warnings(&orders, &resolved.config) {
                eprintln!("summoner: warning: {warning}");
            }
            let problems = order::validate(&orders, &resolved.config);
            if problems.is_empty() {
                println!("{} orders valid", orders.len());
                Ok(0)
            } else {
                for problem in &problems {
                    eprintln!("summoner: {problem}");
                }
                Ok(2)
            }
        }
        Cmd::Plan { paths } => plan::plan(&resolved()?.config, &paths),
        Cmd::Run { paths, stream } => run::run(&resolved()?.config, &paths, stream),
        Cmd::Resume { run_id, stream } => run::resume(&resolved()?.config, &run_id, stream),
        Cmd::Status => {
            let resolved = resolved()?;
            let grove = grove::GroveCli::new(resolved.config.grove_bin());
            let mut status = grove.task_status(&std::env::current_dir()?)?;
            if let Some(tasks) = status.get_mut("tasks").and_then(|t| t.as_array_mut()) {
                tasks.retain(|task| {
                    task["owner"]
                        .as_str()
                        .is_some_and(|owner| owner.starts_with("smn-"))
                });
            }
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(0)
        }
        Cmd::Doctor => doctor(&resolved()?.config),
    }
}

#[derive(Serialize)]
struct DoctorReport {
    grove: DoctorGrove,
    repo: DoctorRepo,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_executor: Option<String>,
    executors: Vec<DoctorExecutor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    ok: bool,
}

/// The charter tells executors to commit, so a repo without a git identity
/// fails every order at the first commit; catch it here instead.
#[derive(Serialize)]
struct DoctorRepo {
    git_repo: bool,
    git_identity: bool,
    ok: bool,
}

#[derive(Serialize)]
struct DoctorGrove {
    bin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DoctorExecutor {
    name: String,
    binary: String,
    found: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    env_missing: Vec<String>,
    timeout_secs: Option<u64>,
    ok: bool,
}

/// Setup problems surface here in seconds — a missing binary or credential is
/// a doctor line, not a 600-second stall inside the first dispatched order.
fn doctor(config: &config::Config) -> Result<i32> {
    let grove_cli = grove::GroveCli::new(config.grove_bin());
    let version = grove_cli.version().ok();
    let grove_report = match grove_cli.preflight() {
        Ok(()) => DoctorGrove {
            bin: config.grove_bin(),
            version,
            ok: true,
            error: None,
        },
        Err(error) => DoctorGrove {
            bin: config.grove_bin(),
            version,
            ok: false,
            error: Some(format!("{error:#}")),
        },
    };

    let git_repo = git_ok(&["rev-parse", "--git-dir"]);
    // `git var` answers the question commit itself asks, including fallbacks
    // (macOS account name); probing config alone rejects working setups.
    let git_identity = git_ok(&["var", "GIT_AUTHOR_IDENT"]);
    let repo = DoctorRepo {
        git_repo,
        git_identity,
        ok: git_repo && git_identity,
    };

    let default_executor = config.default_executor();
    let mut executors = Vec::new();
    for (name, backend) in &config.executors {
        let binary = backend.argv.first().cloned().unwrap_or_default();
        let found = !binary.is_empty() && on_path(&binary);
        let env_missing: Vec<String> = backend
            .env_required
            .iter()
            .filter(|var| std::env::var(var).is_err())
            .cloned()
            .collect();
        let ok = found && env_missing.is_empty();
        executors.push(DoctorExecutor {
            name: name.clone(),
            binary,
            found,
            env_missing,
            timeout_secs: backend.timeout_secs,
            ok,
        });
    }

    // No default executor is a valid setup — every order may name its own.
    // Only a default that points at nothing is a problem.
    let default_ok = match &default_executor {
        Some(name) => config.executors.contains_key(name),
        None => true,
    };
    // Zero executors means nothing can dispatch; say where they belong
    // instead of reporting a hollow success.
    let hint = config.executors.is_empty().then(|| {
        "no executors configured; run `summoner init --global` and define yours in \
         ~/.config/summoner/config.toml (personal) or .summoner.toml (repo override)"
            .to_string()
    });
    let ok = grove_report.ok
        && repo.ok
        && default_ok
        && hint.is_none()
        && executors.iter().all(|executor| executor.ok);
    let report = DoctorReport {
        grove: grove_report,
        repo,
        default_executor,
        executors,
        hint,
        ok,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if report.ok { 0 } else { 1 })
}

fn git_ok(args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

fn on_path(binary: &str) -> bool {
    if binary.contains(std::path::MAIN_SEPARATOR) {
        return executable(Path::new(binary));
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| executable(&dir.join(binary))))
}

/// A present-but-unexecutable file must fail doctor, not the first dispatch.
#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}
