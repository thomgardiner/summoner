//! summoner: fleet runner for LLM coding agents. The invoking session (any
//! harness) is the orchestrator; summoner deterministically dispatches
//! executor CLIs over grove-managed worktrees and reports back.
//!
//! Exit codes: 0 success (for `run`: every order verified), 1 domain outcome
//! needing review, 2 usage or infrastructure error.

mod config;
mod executor;
mod grove;
mod init;
mod order;
mod report;
mod run;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Fleet runner for LLM coding agents on grove worktrees.
#[derive(Parser)]
#[command(name = "summoner", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Drop the orchestration contract: starter config, AGENTS.md section, Claude skill.
    Init,
    /// Print the resolved configuration and where it came from.
    Config,
    /// Validate order files without dispatching anything.
    Check {
        /// Order files or directories of *.toml / *.json orders.
        paths: Vec<PathBuf>,
    },
    /// Execute a fleet of work orders and print the ranked report.
    Run {
        /// Order files or directories of *.toml / *.json orders.
        paths: Vec<PathBuf>,
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
    match Cli::parse().cmd {
        Cmd::Init => {
            let report = init::init(&std::env::current_dir()?)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
        Cmd::Config => {
            println!("{}", serde_json::to_string_pretty(&config::load())?);
            Ok(0)
        }
        Cmd::Check { paths } => {
            let resolved = config::load();
            let orders = order::load(&paths)?;
            for warning in order::warnings(&orders) {
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
        Cmd::Run { paths } => run::run(&config::load().config, &paths),
        Cmd::Status => {
            let resolved = config::load();
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
        Cmd::Doctor => doctor(&config::load().config),
    }
}

#[derive(Serialize)]
struct DoctorReport {
    grove: DoctorGrove,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_executor: Option<String>,
    executors: Vec<DoctorExecutor>,
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

    let default_ok = match &default_executor {
        Some(name) => config.executors.contains_key(name),
        None => false,
    };
    let ok = grove_report.ok && default_ok && executors.iter().all(|executor| executor.ok);
    let report = DoctorReport {
        grove: grove_report,
        default_executor,
        executors,
        ok,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if report.ok { 0 } else { 1 })
}

fn on_path(binary: &str) -> bool {
    if binary.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(binary).exists();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join(binary);
            candidate.is_file()
        })
    })
}
