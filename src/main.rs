//! Model-neutral fleet runner for coding-agent CLIs.

mod assurance_envelope;
mod backend_provenance;
mod config;
mod doctor;
mod drive;
mod events;
mod executor;
mod gate;
mod grove;
mod host;
mod impact;
mod init;
mod integration;
mod land;
mod lifecycle;
mod notify;
mod order;
mod outcome;
mod overview;
mod plan;
mod policy_crypto;
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
mod setup;
mod skills;
mod tripwires;
mod watch;
mod wizard;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "summoner",
    version,
    about = "Host-pluggable fleet runner for coding-agent CLIs (git host by default; Grove optional).",
    after_help = "First run:  summoner setup                 # wizard (pick a model CLI)\n\
                  Or:         summoner setup --preset claude # or codex / kimi\n\
                  Session:    summoner setup --preset kimi --session\n\
                  Project:    summoner setup --repo && summoner init --example\n\
                  Hosts:      [host] kind = \"git\" | \"grove\""
)]
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
    /// First-run ergonomics: skills + executor wizard (no model is pre-selected).
    ///
    /// Run after install. Interactive when a TTY is available; otherwise pass
    /// `--preset`. Session-only recipes do not write permanent global config.
    Setup {
        /// Install a versioned executor recipe (codex, claude, kimi). None → wizard.
        #[arg(long, value_enum)]
        preset: Option<presets::PresetName>,
        /// Apply `--preset` (or wizard choice) only for this session.
        #[arg(long)]
        session: bool,
        /// Force the interactive wizard.
        #[arg(long)]
        wizard: bool,
        /// Remove the session-only config file.
        #[arg(long)]
        clear_session: bool,
        /// Overwrite managed skill files that drifted.
        #[arg(long)]
        refresh: bool,
        /// Also write repo AGENTS.md / contracts in the current directory.
        #[arg(long)]
        repo: bool,
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
    /// Merge a finished run's verified candidates into the current branch, in
    /// dependency order, stopping at the first conflict.
    Land {
        /// The run to land; defaults to the latest finished run.
        run_id: Option<String>,
        /// Print the landing plan without merging anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// One pane across every fleet and every Grove repo on this machine.
    Overview {
        /// Redraw continuously instead of printing once.
        #[arg(long)]
        watch: bool,
    },
    /// Aggregate historical outcomes.
    Scorecard {
        #[arg(long)]
        repo: Option<String>,
    },
    /// Delivery economics vs a baseline scorecard snapshot (honest deltas only).
    Impact {
        /// Baseline JSON from a previous `summoner impact --write-baseline PATH`.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Write current aggregate as a baseline snapshot.
        #[arg(long)]
        write_baseline: Option<PathBuf>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Sign or verify trusted_policy authentication (MAC or ed25519).
    Policy {
        #[command(subcommand)]
        action: PolicyCmd,
    },
    /// Print Summoner-owned Grove tasks.
    Status,
    /// Check Grove, repository identity, selected roles, and optional orders.
    Doctor { paths: Vec<PathBuf> },
}

#[derive(Subcommand)]
enum PolicyCmd {
    /// Generate an ed25519 seed + public key pair (hex).
    Keygen,
    /// Sign the live global trusted_policy body digest.
    ///
    /// Uses `SUMMONER_POLICY_SIGNING_KEY` (ed25519 seed hex) or
    /// `SUMMONER_POLICY_KEY` (legacy MAC secret).
    Sign,
    /// Verify the live global trusted_policy signature.
    Verify,
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
        Cmd::Setup {
            preset,
            session,
            wizard,
            clear_session,
            refresh,
            repo,
        } => {
            let report = setup::setup(
                &std::env::current_dir()?,
                setup::SetupArgs {
                    preset,
                    session,
                    wizard,
                    clear_session,
                    refresh,
                    repo,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(0)
        }
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
            let mut loaded = resolved()?;
            if wizard::needs_executor(&loaded.config) {
                wizard::ensure_executor_or_wizard(&loaded.config)?;
                loaded = resolved()?;
            }
            run::run(
                &loaded.config,
                loaded.selected_profile.as_deref(),
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
        Cmd::Land { run_id, dry_run } => land::land(run_id, dry_run),
        Cmd::Overview { watch } => overview::overview(&resolved()?.config.grove_bin(), watch),
        Cmd::Scorecard { repo } => scorecard::scorecard(repo),
        Cmd::Impact {
            baseline,
            write_baseline,
            repo,
        } => impact::run(
            repo.as_deref(),
            baseline.as_deref(),
            write_baseline.as_deref(),
        ),
        Cmd::Policy { action } => policy_cmd(action),
        Cmd::Status => status(&resolved()?.config),
        Cmd::Doctor { paths } => doctor::run(&resolved()?.config, &paths, cli.allow_unknown_auth),
    }
}

fn policy_cmd(action: PolicyCmd) -> Result<i32> {
    match action {
        PolicyCmd::Keygen => {
            let (seed, pubkey) = policy_crypto::generate_keypair()?;
            println!(
                "{}",
                serde_json::json!({
                    "scheme": "ed25519",
                    "signing_seed_hex": seed,
                    "public_key_hex": pubkey,
                    "env": {
                        "SUMMONER_POLICY_SIGNING_KEY": seed,
                        "SUMMONER_POLICY_PUBKEY": pubkey,
                    },
                    "note": "store the seed privately; publish only the public key"
                })
            );
            Ok(0)
        }
        PolicyCmd::Sign => {
            let policy = resolved_policy()?;
            let digest = policy.sha256();
            let signature = if let Ok(seed) = std::env::var("SUMMONER_POLICY_SIGNING_KEY") {
                policy_crypto::sign_ed25519(seed.as_bytes(), &digest)?
            } else if let Ok(key) = std::env::var("SUMMONER_POLICY_KEY") {
                config::TrustedPolicy::mac_hex(key.as_bytes(), &digest)
            } else {
                anyhow::bail!(
                    "set SUMMONER_POLICY_SIGNING_KEY (ed25519 seed hex) or SUMMONER_POLICY_KEY (MAC secret)"
                );
            };
            println!(
                "{}",
                serde_json::json!({
                    "policy_sha256": digest,
                    "signature": signature,
                    "hint": "paste signature into [trusted_policy].signature in global config"
                })
            );
            Ok(0)
        }
        PolicyCmd::Verify => {
            let policy = resolved_policy()?;
            let valid = policy.verify_signature()?;
            println!(
                "{}",
                serde_json::json!({
                    "policy_sha256": policy.sha256(),
                    "signature_present": policy.signature.is_some(),
                    "signature_valid": valid,
                    "identity": policy.identity(valid),
                })
            );
            Ok(if valid == Some(false) { 1 } else { 0 })
        }
    }
}

fn resolved_policy() -> Result<config::TrustedPolicy> {
    let resolved = config::load()?;
    resolved
        .config
        .trusted_policy
        .clone()
        .context("no [trusted_policy] in resolved config; set it in the personal global config")
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
    // Global preset path also drops user skills so older docs still land
    // harness invoke without a separate setup call.
    let skills = if global || preset.is_some() || example {
        Some(skills::install_user_skills(refresh)?)
    } else {
        None
    };
    let next_steps: Vec<String> = if example {
        vec![
            "summoner doctor orders/example.toml".into(),
            "summoner plan orders/example.toml".into(),
            "summoner run --stream orders/example.toml".into(),
        ]
    } else if global || preset.is_some() {
        // Prefer the setup-oriented doctor nudge over the long skill list for
        // init --global (tests and first-run both read this).
        vec!["summoner doctor".into()]
    } else {
        vec![
            "summoner doctor".into(),
            "For harness slash/skill install: summoner setup".into(),
        ]
    };
    let _ = skills; // installed above when global/example; details in setup command
    #[derive(Serialize)]
    struct Output {
        #[serde(flatten)]
        report: init::Report,
        #[serde(skip_serializing_if = "Option::is_none")]
        skills: Option<skills::Report>,
        next_steps: Vec<String>,
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            report,
            skills,
            next_steps
        })?
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
