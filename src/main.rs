//! Model-neutral fleet runner for coding-agent CLIs.

mod backend_provenance;
mod config;
mod doctor;
mod drive;
mod events;
mod executor;
mod gate;
mod grove;
mod init;
mod integration;
mod lifecycle;
mod notify;
mod order;
mod outcome;
mod plan;
mod presets;
mod report;
mod review;
mod review_gate;
mod review_protocol;
mod review_worker;
mod run;
mod run_evidence;
mod run_journal;
mod run_manifest;
mod run_prepare;
mod run_resume;
mod scorecard;
mod tripwires;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "summoner", version)]
struct Cli {
    /// Orchestrator profile from `[profiles.<name>]` in the config.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Explicitly accept a healthy executor whose CLI cannot prove auth.
    #[arg(long, global = true)]
    allow_unknown_auth: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(name = "__review-worker", hide = true)]
    ReviewWorker {
        #[arg(long)]
        prompt_file: PathBuf,
        #[arg(long)]
        stdin: bool,
        #[arg(long)]
        expected_path: String,
        #[arg(long)]
        expected_sha256: String,
        #[arg(long)]
        expected_prompt_sha256: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Initialize repository contracts, a global preset, or a demo order.
    Init {
        /// Write personal executor configuration in the native config directory.
        #[arg(long, conflicts_with = "refresh")]
        global: bool,
        /// Explicit global model preset. Never auto-detected.
        #[arg(long, value_enum, conflicts_with = "refresh")]
        preset: Option<presets::PresetName>,
        /// Add a small orders/example.toml alongside normal repo initialization.
        #[arg(long, conflicts_with = "refresh")]
        example: bool,
        /// Refresh Summoner-managed AGENTS.md and skill content.
        #[arg(long, conflicts_with_all = ["global", "preset", "example"])]
        refresh: bool,
    },
    /// Print the resolved configuration and where it came from.
    Config,
    /// Validate order files without dispatching anything.
    Check { paths: Vec<PathBuf> },
    /// Analyze claim conflicts, couplings, waves, and dependency edges.
    Plan { paths: Vec<PathBuf> },
    /// Execute a fleet and print its ranked report.
    Run {
        paths: Vec<PathBuf>,
        #[arg(long)]
        stream: bool,
    },
    /// Re-run non-green work from an earlier run.
    Resume {
        run_id: String,
        #[arg(long)]
        stream: bool,
    },
    /// Watch the latest or named run.
    Watch { run_id: Option<String> },
    /// Aggregate historical outcomes.
    Scorecard {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Print Summoner-owned Grove tasks.
    Status,
    /// Check Grove, repository identity, selected roles, and optional orders.
    Doctor { paths: Vec<PathBuf> },
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
        let mut resolved = config::load()?;
        if let Some(name) = config::select_profile(&mut resolved.config, cli.profile.as_deref())? {
            resolved.sources.push(format!("profile {name}"));
            resolved.selected_profile = Some(name);
        }
        Ok(resolved)
    };
    match cli.cmd {
        Cmd::ReviewWorker {
            prompt_file,
            stdin,
            expected_path,
            expected_sha256,
            expected_prompt_sha256,
            command,
        } => review_worker::run(
            &prompt_file,
            stdin,
            &expected_path,
            &expected_sha256,
            &expected_prompt_sha256,
            &command,
        ),
        Cmd::Init {
            global,
            preset,
            example,
            refresh,
        } => initialize(global, preset, example, refresh),
        Cmd::Config => {
            println!("{}", serde_json::to_string_pretty(&resolved()?)?);
            Ok(0)
        }
        Cmd::Check { paths } => check(&resolved()?.config, &paths),
        Cmd::Plan { paths } => plan::plan(&resolved()?.config, &paths),
        Cmd::Run { paths, stream } => {
            let resolved = resolved()?;
            run::run(
                &resolved.config,
                resolved.selected_profile.as_deref(),
                &paths,
                stream,
                cli.allow_unknown_auth,
            )
        }
        Cmd::Resume { run_id, stream } => {
            let resolved = resolved()?;
            run::resume(
                &resolved.config,
                resolved.selected_profile.as_deref(),
                &run_id,
                stream,
                cli.allow_unknown_auth,
            )
        }
        Cmd::Watch { run_id } => watch::watch(run_id),
        Cmd::Scorecard { repo } => scorecard::scorecard(repo),
        Cmd::Status => status(&resolved()?.config),
        Cmd::Doctor { paths } => doctor::run(&resolved()?.config, &paths, cli.allow_unknown_auth),
    }
}

fn initialize(
    global: bool,
    preset: Option<presets::PresetName>,
    example: bool,
    refresh: bool,
) -> Result<i32> {
    let report = if example {
        init::onboard(&std::env::current_dir()?, global, preset)?
    } else if global || preset.is_some() {
        init::global(preset)?
    } else {
        init::init(&std::env::current_dir()?, refresh)?
    };
    let next_steps = if example {
        vec![
            "summoner doctor orders/example.toml",
            "summoner plan orders/example.toml",
            "summoner run --stream orders/example.toml",
        ]
    } else {
        vec!["summoner doctor"]
    };
    #[derive(Serialize)]
    struct Output {
        #[serde(flatten)]
        report: init::Report,
        next_steps: Vec<&'static str>,
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Output { report, next_steps })?
    );
    Ok(0)
}

fn check(config: &config::Config, paths: &[PathBuf]) -> Result<i32> {
    let orders = order::load(paths)?;
    for warning in order::warnings(&orders, config) {
        eprintln!("summoner: warning: {warning}");
    }
    let problems = order::validate(&orders, config);
    if problems.is_empty() {
        println!("{} orders valid", orders.len());
        return Ok(0);
    }
    for problem in &problems {
        eprintln!("summoner: {problem}");
    }
    Ok(2)
}

fn status(config: &config::Config) -> Result<i32> {
    let grove = grove::GroveCli::new(config.grove_bin());
    let mut status = grove.task_status(&std::env::current_dir()?)?;
    if let Some(tasks) = status
        .get_mut("tasks")
        .and_then(|tasks| tasks.as_array_mut())
    {
        tasks.retain(|task| {
            task["owner"]
                .as_str()
                .is_some_and(|owner| owner.starts_with("smn-"))
        });
    }
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(0)
}
