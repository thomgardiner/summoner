mod candidate;
mod config;
mod executor;
mod git;
mod order;
mod process;
mod report;
mod run;
mod verify;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "summoner",
    version,
    about = "Run coding-agent orders in git worktrees"
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    Check {
        paths: Vec<PathBuf>,
    },
    Plan {
        paths: Vec<PathBuf>,
    },
    Doctor {
        paths: Vec<PathBuf>,
    },
    Run {
        #[arg(long)]
        stream: bool,
        #[arg(long)]
        jobs: Option<usize>,
        paths: Vec<PathBuf>,
    },
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
    match cli.command {
        CommandKind::Check { paths } => {
            let config = config::load(cli.config.as_deref())?;
            let orders = order::load(&paths)?;
            order::validate(&orders, &config)?;
            println!(
                "{}",
                serde_json::json!({"ok": true, "orders": orders.iter().map(|o| &o.id).collect::<Vec<_>>() })
            );
            Ok(0)
        }
        CommandKind::Plan { paths } => {
            let config = config::load(cli.config.as_deref())?;
            let orders = order::load(&paths)?;
            order::validate(&orders, &config)?;
            let waves = order::topological_waves(&orders).context("dependency cycle")?;
            println!("{}", serde_json::json!({"waves": waves}));
            Ok(0)
        }
        CommandKind::Doctor { paths } => doctor(cli.config.as_deref(), &paths),
        CommandKind::Run {
            stream,
            jobs,
            paths,
        } => {
            let config = config::load(cli.config.as_deref())?;
            let orders = order::load(&paths)?;
            order::validate(&orders, &config)?;
            let jobs = config::max_parallel(&config, jobs)?;
            let repo = git::repo_root()?;
            let report = run::run_fleet(&repo, config, orders, jobs, stream)?;
            if stream {
                println!(
                    "{}",
                    serde_json::json!({"event": "report", "report": report})
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            Ok(report.exit_code())
        }
    }
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    git: bool,
    executors: Vec<CheckEntry>,
    verification: Vec<CheckEntry>,
    orders: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CheckEntry {
    name: String,
    argv: Vec<String>,
    available: bool,
    missing_env: Vec<String>,
}

fn doctor(config_path: Option<&std::path::Path>, paths: &[PathBuf]) -> Result<i32> {
    let config = config::load(config_path)?;
    let (orders, executor_names, profile_names) = if paths.is_empty() {
        (
            Vec::new(),
            config.executors.keys().cloned().collect(),
            config.verification.profiles.keys().cloned().collect(),
        )
    } else {
        let orders = order::load(paths)?;
        order::validate(&orders, &config)?;
        let executor_names = orders
            .iter()
            .filter_map(|order| {
                order
                    .executor
                    .as_deref()
                    .or(config.default_executor.as_deref())
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let profile_names = orders
            .iter()
            .filter_map(|order| {
                order
                    .verify_profile
                    .as_deref()
                    .or(config.default_verify_profile.as_deref())
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        (orders, executor_names, profile_names)
    };
    let git_ok = process::executable_available(&["git".into()]);
    let executors: Vec<_> = executor_names
        .iter()
        .filter_map(|name| config.executors.get(name).map(|executor| (name, executor)))
        .map(|(name, executor)| CheckEntry {
            name: name.clone(),
            argv: executor.argv.clone(),
            available: process::executable_available(&executor.argv),
            missing_env: executor
                .env_required
                .iter()
                .filter(|key| std::env::var_os(key).is_none())
                .cloned()
                .collect(),
        })
        .collect();
    let verification = profile_names
        .iter()
        .filter_map(|name| {
            config
                .verification
                .profiles
                .get(name)
                .map(|profile| (name, profile))
        })
        .flat_map(|(name, profile)| {
            profile
                .commands
                .iter()
                .enumerate()
                .map(move |(index, command)| (name, index, command))
        })
        .map(|(name, index, command)| CheckEntry {
            name: format!("{name}[{index}]"),
            argv: command.argv().to_vec(),
            available: process::executable_available(command.argv()),
            missing_env: Vec::new(),
        })
        .collect::<Vec<_>>();
    let ok = git_ok
        && executors
            .iter()
            .all(|entry| entry.available && entry.missing_env.is_empty())
        && verification.iter().all(|entry| entry.available);
    let report = DoctorReport {
        ok,
        git: git_ok,
        executors,
        verification,
        orders: orders.into_iter().map(|order| order.id).collect(),
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if ok { 0 } else { 1 })
}
