//! The fleet loop: validated orders in, one ranked report out. Every order
//! walks the same state machine, and every arm converges on the same tail —
//! collect evidence, terminalize the grove task, release the worktree, report.

use crate::config::Config;
use crate::executor::{self, ExecRequest};
use crate::grove::{BeginOutcome, FinishOutcome, GroveCli};
use crate::order::{self, Order};
use crate::report::{DiffStats, OrderReport, Outcome, RunReport};
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

const TAIL_BYTES: usize = 2048;

struct Ctx<'a> {
    config: &'a Config,
    grove: GroveCli,
    repo: PathBuf,
    run_dir: PathBuf,
}

pub fn run(config: &Config, paths: &[PathBuf]) -> Result<i32> {
    let grove = GroveCli::new(config.grove_bin());
    grove.preflight()?;

    let orders = order::load(paths)?;
    for warning in order::warnings(&orders) {
        eprintln!("summoner: warning: {warning}");
    }
    let problems = order::validate(&orders, config);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("summoner: {problem}");
        }
        bail!("{} order problem(s); nothing dispatched", problems.len());
    }
    preflight_env(&orders, config)?;

    let repo = std::env::current_dir().context("resolving current directory")?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_id = format!("{started_at}-{}", std::process::id());
    let run_dir = runs_root().join(&run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating run dir {}", run_dir.display()))?;

    install_interrupt_handler();

    let ctx = Ctx {
        config,
        grove,
        repo: repo.clone(),
        run_dir: run_dir.clone(),
    };
    let started = Instant::now();
    let workers = config.max_parallel().min(orders.len().max(1));
    let queue = Mutex::new(orders.into_iter().collect::<VecDeque<_>>());
    let results = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let Some(order) = queue.lock().unwrap().pop_front() else {
                        break;
                    };
                    let report = if SHUTDOWN.load(Ordering::SeqCst) {
                        skipped(&order, ctx.config)
                    } else {
                        run_order(&ctx, &order)
                    };
                    results.lock().unwrap().push(report);
                }
            });
        }
    });

    let report = RunReport::assemble(
        run_id,
        repo.display().to_string(),
        started_at,
        started.elapsed().as_secs(),
        results.into_inner().unwrap(),
    );
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(run_dir.join("report.json"), &json).context("writing report.json")?;
    println!("{json}");
    Ok(report.exit_code())
}

/// Missing executor environment fails in seconds with the fix named, not after
/// a full timeout inside the first order.
fn preflight_env(orders: &[Order], config: &Config) -> Result<()> {
    let mut missing = Vec::new();
    let mut checked = std::collections::BTreeSet::new();
    for order in orders {
        let Some(name) = order.executor_name(config) else {
            continue;
        };
        if !checked.insert(name.clone()) {
            continue;
        }
        if let Some(backend) = config.executors.get(&name) {
            for var in &backend.env_required {
                if std::env::var(var).is_err() {
                    missing.push(format!(
                        "executor {name:?} needs ${var} (interactive-shell exports do not reach \
                         summoner; export it here or persist it via the backend's auth flow)"
                    ));
                }
            }
        }
    }
    if !missing.is_empty() {
        bail!("{}", missing.join("\n"));
    }
    Ok(())
}

fn run_order(ctx: &Ctx, order: &Order) -> OrderReport {
    let executor_name = order
        .executor_name(ctx.config)
        .expect("validated before dispatch");
    let mut report = OrderReport::new(order, executor_name.clone());
    let total = Instant::now();
    if let Err(error) = drive(ctx, order, &executor_name, &mut report) {
        report.outcome = Outcome::Error;
        report.detail = Some(format!("{error:#}"));
        // Best effort: never leak a claim on the error path.
        if let (Some(task_id), Some(worktree)) = (&report.task_id, &report.worktree) {
            let _ =
                ctx.grove
                    .task_abandon(Path::new(worktree), task_id, "summoner: internal error");
        }
        if let Some(worktree) = report.worktree.clone() {
            release(ctx, Path::new(&worktree), &mut report);
        }
    }
    report.timing.total_secs = total.elapsed().as_secs();
    report
}

/// The state machine. Sets `report.outcome` on every path; returns Err only
/// for summoner-side failures that map to `error`.
fn drive(ctx: &Ctx, order: &Order, executor_name: &str, report: &mut OrderReport) -> Result<()> {
    let backend = &ctx.config.executors[executor_name];
    let agent = order.agent();

    let worktree = ctx.grove.worktree_acquire(
        &ctx.repo,
        &agent,
        order.branch.as_deref(),
        order.base.as_deref(),
    )?;
    report.worktree = Some(worktree.display().to_string());
    report.branch = git(&worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    report.base_commit = git(&worktree, &["rev-parse", "HEAD"]).ok();

    match ctx
        .grove
        .task_begin(&worktree, &agent, &order.title, &order.scope)?
    {
        BeginOutcome::Begun { task } => report.task_id = Some(task.id),
        BeginOutcome::Conflict { conflicts } => {
            report.outcome = Outcome::Blocked;
            report.conflicts = Some(serde_json::Value::Array(conflicts));
            release(ctx, &worktree, report);
            return Ok(());
        }
    }
    let task_id = report.task_id.clone().expect("just set");

    let timeout_secs = order
        .timeout_secs
        .or(backend.timeout_secs)
        .unwrap_or_else(|| ctx.config.order_timeout_secs());
    let order_dir = ctx.run_dir.join(&order.id);
    report.stdout_log = Some(order_dir.join("stdout.log").display().to_string());
    report.stderr_log = Some(order_dir.join("stderr.log").display().to_string());

    let exec_started = Instant::now();
    let exec = executor::run_executor(&ExecRequest {
        grove: &ctx.grove,
        backend,
        order,
        task_id: &task_id,
        worktree: &worktree,
        run_dir: &order_dir,
        timeout_secs,
        shutdown: &SHUTDOWN,
    })?;
    report.timing.exec_secs = exec_started.elapsed().as_secs();
    report.executor_exit = exec.exit;

    let interrupted = SHUTDOWN.load(Ordering::SeqCst);
    let (outcome, abandon_reason) = if exec.backup_killed {
        (
            Some(Outcome::Stalled),
            "summoner: backup deadline fired; grove supervisor did not return",
        )
    } else if interrupted {
        (
            Some(Outcome::Interrupted),
            "summoner: interrupted by operator",
        )
    } else {
        match exec.exit {
            Some(124) => (Some(Outcome::Stalled), "summoner: executor timeout"),
            Some(0) => (None, ""),
            code => {
                report.detail = Some(format!("executor exit {code:?}"));
                (Some(Outcome::ExecutorFailed), "summoner: executor failed")
            }
        }
    };

    if let Some(outcome) = outcome {
        report.outcome = outcome;
        finalize(
            ctx,
            order,
            &task_id,
            &worktree,
            report,
            Some(abandon_reason),
        );
        return Ok(());
    }

    // Verification, finish-driven: run the order's profile, attempt finish,
    // then run exactly what the refusal names before one retry.
    let verify_started = Instant::now();
    verification(ctx, order, &task_id, &worktree, report)?;
    report.timing.verify_secs = verify_started.elapsed().as_secs();

    let abandon = match report.outcome {
        Outcome::Verified | Outcome::Completed => None,
        Outcome::ScopeViolation => Some("summoner: writes outside declared scope"),
        _ => Some("summoner: verification failed"),
    };
    finalize(ctx, order, &task_id, &worktree, report, abandon);
    Ok(())
}

fn verification(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
) -> Result<()> {
    let mut ran = std::collections::BTreeSet::new();
    if let Some(profile) = order
        .verify_profile
        .clone()
        .or_else(|| ctx.config.default_verify_profile.clone())
    {
        let summary = ctx.grove.verify(worktree, &profile, task_id)?;
        let passed = summary.passed;
        ran.insert(profile.clone());
        report.verify.push(summary);
        if !passed {
            report.outcome = Outcome::Unverified;
            report.detail = Some(format!("verify profile {profile:?} failed"));
            return Ok(());
        }
    }

    for attempt in 0..2 {
        match ctx.grove.task_finish(worktree, task_id, None)? {
            FinishOutcome::Finished { verification } => {
                report.finish = Some(verification);
                report.outcome = Outcome::Verified;
                return Ok(());
            }
            FinishOutcome::Refused {
                reason,
                outside_scope,
                verification,
            } => {
                if reason == "scope" {
                    report.outcome = Outcome::ScopeViolation;
                    report.detail = Some(format!("out of scope: {}", outside_scope.join(", ")));
                    return Ok(());
                }
                let verification = verification.unwrap_or_default();
                let wanted: Vec<String> = verification
                    .missing
                    .iter()
                    .chain(verification.stale.iter())
                    .filter(|profile| !ran.contains(*profile))
                    .cloned()
                    .collect();
                if verification.required.is_empty() {
                    // The repository declares no required profiles; grove can
                    // never mark this verified. Finish with the override on
                    // the record and report it as completed, not verified.
                    let reason = "summoner: repository declares no required verification profiles";
                    if let FinishOutcome::Finished { verification } =
                        ctx.grove.task_finish(worktree, task_id, Some(reason))?
                    {
                        report.finish = Some(verification);
                        report.outcome = Outcome::Completed;
                        report.detail = Some(reason.to_string());
                        return Ok(());
                    }
                    report.outcome = Outcome::Unverified;
                    report.detail = Some("finish refused the explicit override".into());
                    return Ok(());
                }
                if attempt == 1 || wanted.is_empty() {
                    report.finish = Some(verification);
                    report.outcome = Outcome::Unverified;
                    report.detail = Some("finish refused: required evidence not fresh".into());
                    return Ok(());
                }
                for profile in wanted {
                    let summary = ctx.grove.verify(worktree, &profile, task_id)?;
                    let passed = summary.passed;
                    ran.insert(profile.clone());
                    report.verify.push(summary);
                    if !passed {
                        report.outcome = Outcome::Unverified;
                        report.detail = Some(format!("required profile {profile:?} failed"));
                        return Ok(());
                    }
                }
            }
        }
    }
    unreachable!("finish loop returns within two attempts");
}

/// The convergent tail: abandon a non-terminal task, collect diff evidence,
/// read log tails, release (or deliberately keep) the worktree.
fn finalize(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    abandon_reason: Option<&str>,
) {
    if let Some(reason) = abandon_reason
        && let Err(error) = ctx.grove.task_abandon(worktree, task_id, reason)
    {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; abandon failed: {error:#}"),
            None => format!("abandon failed: {error:#}"),
        });
    }

    if let Some(base) = report.base_commit.clone() {
        report.commits = git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        report.diff = Some(diff_stats(worktree, &base));
    }
    if let Some(path) = &report.stdout_log {
        report.stdout_tail = executor::tail(Path::new(path), TAIL_BYTES);
    }
    if let Some(path) = &report.stderr_log {
        report.stderr_tail = executor::tail(Path::new(path), TAIL_BYTES);
    }

    let keep = ctx.config.keep_failed_worktrees()
        && !matches!(report.outcome, Outcome::Verified | Outcome::Completed);
    if keep {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; worktree kept for post-mortem"),
            None => "worktree kept for post-mortem".to_string(),
        });
    } else {
        release(ctx, worktree, report);
    }
    let _ = order;
}

fn release(ctx: &Ctx, worktree: &Path, report: &mut OrderReport) {
    match ctx.grove.worktree_release(&ctx.repo, worktree) {
        Ok(outcome) => {
            report.saved_to = outcome.saved_to;
            if report.branch.is_none() {
                report.branch = outcome.branch;
            }
        }
        // Reap will NOT clean a checkout that left its branch; say so plainly.
        Err(error) => {
            report.release_error = Some(format!("{error:#}; needs manual recovery"));
        }
    }
}

fn skipped(order: &Order, config: &Config) -> OrderReport {
    let executor = order.executor_name(config).unwrap_or_default();
    let mut report = OrderReport::new(order, executor);
    report.outcome = Outcome::Skipped;
    report.detail = Some("not started: run interrupted".into());
    report
}

fn diff_stats(worktree: &Path, base: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    if let Ok(shortstat) = git(worktree, &["diff", "--shortstat", &format!("{base}..HEAD")]) {
        for part in shortstat.split(',') {
            let number: u64 = part
                .trim()
                .split(' ')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if part.contains("file") {
                stats.files_changed = number;
            } else if part.contains("insertion") {
                stats.insertions = number;
            } else if part.contains("deletion") {
                stats.deletions = number;
            }
        }
    }
    if let Ok(porcelain) = git(worktree, &["status", "--porcelain"]) {
        stats.uncommitted_files = porcelain.lines().filter(|l| !l.is_empty()).count() as u64;
    }
    stats
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn runs_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("summoner")
        .join("runs")
}

#[cfg(unix)]
fn install_interrupt_handler() {
    extern "C" fn note_interrupt(_: libc::c_int) {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(
            libc::SIGINT,
            note_interrupt as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            note_interrupt as *const () as libc::sighandler_t,
        );
    }
}

#[cfg(not(unix))]
fn install_interrupt_handler() {}
