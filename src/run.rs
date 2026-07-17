//! The fleet loop: validated orders in, one ranked report out. Every order
//! walks the same state machine, and every arm converges on the same tail —
//! collect evidence, terminalize the grove task, release the worktree, report.

use crate::config::Config;
use crate::events::EventSink;
use crate::executor::{self, ExecRequest};
use crate::grove::{BeginOutcome, FinishOutcome, GroveCli};
use crate::order::{self, Order};
use crate::report::{DiffStats, OrderReport, Outcome, ReviewSummary, RunReport};
use crate::review;
use crate::tripwires;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// One panicked worker must not poison the whole fleet: the guarded data
/// (queue, report list) keeps its invariants per-operation, so recover the
/// lock and keep collecting the other orders' reports.
fn relock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const TAIL_BYTES: usize = 2048;

struct Ctx<'a> {
    config: &'a Config,
    grove: GroveCli,
    repo: PathBuf,
    run_dir: PathBuf,
    events: EventSink,
}

pub fn run(config: &Config, paths: &[PathBuf], stream: bool) -> Result<i32> {
    let grove = GroveCli::new(config.grove_bin());
    grove.preflight()?;
    let orders = validated(paths, config)?;
    execute(config, grove, orders, stream, Vec::new())
}

/// Re-run an earlier fleet. Orders that already reached a successful outcome
/// are carried into the new report verbatim; everything else dispatches again
/// on its original branch, continuing from whatever grove salvaged of the
/// previous attempt (acquire onto an existing branch resumes it).
pub fn resume(config: &Config, run_id: &str, stream: bool) -> Result<i32> {
    let grove = GroveCli::new(config.grove_bin());
    grove.preflight()?;

    let report_path = runs_root().join(run_id).join("report.json");
    let prior: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&report_path)
            .with_context(|| format!("no report for run {run_id} at {}", report_path.display()))?,
    )
    .context("parsing the prior run report")?;
    let prior_orders = prior["orders"].as_array().cloned().unwrap_or_default();
    // Deduplicate: variant siblings all report the original order file, and
    // loading it twice would expand duplicate sibling ids that fail validation.
    let mut seen_files = std::collections::BTreeSet::new();
    let files: Vec<PathBuf> = prior_orders
        .iter()
        .filter_map(|entry| entry["order_file"].as_str().map(PathBuf::from))
        .filter(|path| seen_files.insert(path.clone()))
        .collect();
    if files.is_empty() {
        bail!("run {run_id} names no order files to resume");
    }
    let orders = validated(&files, config)?;

    let mut carried_outcomes = BTreeMap::new();
    let mut prior_branches = BTreeMap::new();
    for entry in &prior_orders {
        let Some(id) = entry["id"].as_str() else {
            continue;
        };
        if let Some(branch) = entry["branch"].as_str() {
            prior_branches.insert(id.to_string(), branch.to_string());
        }
        if let Some(outcome) = entry["outcome"].as_str().and_then(Outcome::from_key)
            && matches!(
                outcome,
                Outcome::Verified | Outcome::Completed | Outcome::Approved
            )
        {
            carried_outcomes.insert(id.to_string(), outcome);
        }
    }
    let (carried_orders, mut to_run): (Vec<Order>, Vec<Order>) = orders
        .into_iter()
        .partition(|order| carried_outcomes.contains_key(&order.id));
    for order in &mut to_run {
        // Pin the prior attempt's branch explicitly: grove reuses a branch it
        // is told about, but derives a fresh suffixed name when the default is
        // taken — which would silently abandon the salvaged work.
        if order.branch.is_none()
            && let Some(branch) = prior_branches.get(&order.id)
        {
            order.branch = Some(branch.clone());
        }
    }
    let carried = carried_orders
        .iter()
        .map(|order| {
            let mut report =
                OrderReport::new(order, order.executor_name(config).unwrap_or_default());
            report.outcome = carried_outcomes[&order.id];
            report.detail = Some(format!("carried from run {run_id}"));
            report.branch = prior_branches.get(&order.id).cloned();
            report
        })
        .collect();
    execute(config, grove, to_run, stream, carried)
}

/// Load, warn, and fail-fast validate a batch before anything is dispatched.
fn validated(paths: &[PathBuf], config: &Config) -> Result<Vec<Order>> {
    let orders = order::load(paths)?;
    for warning in order::warnings(&orders, config) {
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
    Ok(orders)
}

fn execute(
    config: &Config,
    grove: GroveCli,
    orders: Vec<Order>,
    stream: bool,
    carried: Vec<OrderReport>,
) -> Result<i32> {
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

    let events = EventSink::new(&run_dir, stream);
    let workers = config.max_parallel().min(orders.len().max(1));
    events.emit(
        "run_started",
        serde_json::json!({
            "run_id": run_id,
            "run_dir": run_dir.display().to_string(),
            "repo": repo.display().to_string(),
            "workers": workers,
            "orders": orders.iter().map(|o| o.id.clone()).collect::<Vec<_>>(),
            "carried": carried.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        }),
    );
    let ctx = Ctx {
        config,
        grove,
        repo: repo.clone(),
        run_dir: run_dir.clone(),
        events,
    };
    let started = Instant::now();
    let mut scheduler = Scheduler::new(orders, config.fail_fast());
    // Carried orders count as done so their dependents dispatch immediately.
    for prior in &carried {
        scheduler.complete(&prior.id, prior.outcome);
    }
    let scheduler = Mutex::new(scheduler);
    let results = Mutex::new(carried);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = {
                        let mut scheduler = relock(&scheduler);
                        if SHUTDOWN.load(Ordering::SeqCst) {
                            scheduler.drain()
                        } else {
                            scheduler.next()
                        }
                    };
                    let (order, report) = match next {
                        Next::Done => break,
                        Next::Wait => {
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        Next::Skip(order, reason) => {
                            let report = skipped(&order, ctx.config, reason);
                            (order, report)
                        }
                        Next::Run(order) => {
                            let report = run_order(&ctx, &order);
                            (order, report)
                        }
                    };
                    ctx.events.emit(
                        "order_finished",
                        serde_json::json!({
                            "id": report.id,
                            "outcome": report.outcome.key(),
                            "detail": report.detail,
                            "usage_tokens": report.usage_tokens,
                        }),
                    );
                    relock(&scheduler).complete(&order.id, report.outcome);
                    relock(&results).push(report);
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
    ctx.events.emit(
        "run_finished",
        serde_json::json!({
            "run_id": report.run_id,
            "duration_secs": report.duration_secs,
            "summary": report.summary,
            "usage_tokens": report.usage_tokens,
            "exit_code": report.exit_code(),
            "report_path": run_dir.join("report.json").display().to_string(),
        }),
    );
    if ctx.events.streaming() {
        // Stream consumers get the complete report as the final NDJSON line;
        // the pretty print would break line-oriented parsers.
        println!(
            "{}",
            serde_json::json!({"event": "report", "report": &report})
        );
    } else {
        println!("{json}");
    }
    Ok(report.exit_code())
}

/// Missing executor environment fails in seconds with the fix named, not after
/// a full timeout inside the first order.
fn preflight_env(orders: &[Order], config: &Config) -> Result<()> {
    let mut missing = Vec::new();
    let mut checked = std::collections::BTreeSet::new();
    for order in orders {
        let names = [order.executor_name(config), order.reviewer_name(config)];
        for name in names.into_iter().flatten() {
            if !checked.insert(name.clone()) {
                continue;
            }
            if let Some(backend) = config.executors.get(&name) {
                for var in &backend.env_required {
                    if std::env::var(var).is_err() {
                        missing.push(format!(
                            "executor {name:?} needs ${var} (interactive-shell exports do not \
                             reach summoner; export it here or persist it via the backend's \
                             auth flow)"
                        ));
                    }
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
    ctx.events.emit(
        "order_started",
        serde_json::json!({"id": order.id, "executor": executor_name}),
    );
    let mut report = OrderReport::new(order, executor_name.clone());
    let total = Instant::now();
    if let Err(error) = drive(ctx, order, &executor_name, &mut report) {
        report.outcome = Outcome::Error;
        report.detail = Some(format!("{error:#}"));
        // Never leak a claim on the error path; a failed abandon means one may
        // remain live, and the report must say so.
        if let (Some(task_id), Some(worktree)) = (&report.task_id, &report.worktree)
            && let Err(abandon_error) =
                ctx.grove
                    .task_abandon(Path::new(worktree), task_id, "summoner: internal error")
        {
            report.detail = Some(format!(
                "{}; abandon failed, claim may still be live: {abandon_error:#}",
                report.detail.take().unwrap_or_default()
            ));
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

    match ctx.grove.task_begin(
        &worktree,
        &agent,
        &order.title,
        &order.scope,
        order.claim_group.as_deref(),
    )? {
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

    // No --path-format=absolute: that flag needs git >= 2.31, and absolutizing
    // a relative answer against the worktree is version-proof.
    let git_common_dir = {
        let raw = PathBuf::from(git(&worktree, &["rev-parse", "--git-common-dir"])?);
        if raw.is_absolute() {
            raw
        } else {
            worktree.join(raw)
        }
    };
    // Everything a live consumer needs to follow this order: the task to
    // poll, the worktree to inspect, and the logs to tail.
    ctx.events.emit(
        "order_dispatched",
        serde_json::json!({
            "id": order.id,
            "task_id": task_id,
            "worktree": report.worktree,
            "branch": report.branch,
            "stdout_log": report.stdout_log,
            "stderr_log": report.stderr_log,
            "timeout_secs": timeout_secs,
        }),
    );
    let exec_started = Instant::now();
    let prompt = executor::compose_prompt(order);
    let exec = executor::run_executor(&ExecRequest {
        grove: &ctx.grove,
        backend,
        order,
        task_id: &task_id,
        worktree: &worktree,
        git_common_dir: &git_common_dir,
        run_dir: &order_dir,
        timeout_secs,
        shutdown: &SHUTDOWN,
        prompt: &prompt,
        file_prefix: "",
    })?;
    report.timing.exec_secs = exec_started.elapsed().as_secs();
    report.executor_exit = exec.exit;
    if let Some(marker) = &backend.usage_marker {
        report.usage_tokens = ["stderr.log", "stdout.log"].iter().find_map(|name| {
            executor::tail(&order_dir.join(name), 8192)
                .as_deref()
                .and_then(|text| number_after(text, marker))
        });
    }
    ctx.events.emit(
        "order_exec_done",
        serde_json::json!({
            "id": order.id,
            "exit": exec.exit,
            "backup_killed": exec.backup_killed,
            "usage_tokens": report.usage_tokens,
        }),
    );

    let interrupted = SHUTDOWN.load(Ordering::SeqCst);
    let (outcome, abandon_reason) = if exec.backup_killed {
        // grove itself was wedged, so its deadline never fired — but its task
        // record still names the executor process-group leader. Kill that
        // group directly: a stuck fleet must not keep spending executor
        // budget unsupervised.
        kill_recorded_group(ctx, &task_id, &worktree);
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

    // Deterministic anti-hacking scan before any evidence is trusted. A
    // modified verification config invalidates the receipts it would produce,
    // so that is a hard stop; soft flags ride along to the reviewer. A scan
    // that cannot collect evidence propagates as `error`: the gate never
    // reports a clean pass it did not perform.
    let base = report.base_commit.clone().unwrap_or_else(|| "HEAD".into());
    let trips = tripwires::scan(&worktree, &base)?;
    report.tripwires = trips.flags.clone();
    if !trips.protected.is_empty() {
        report.outcome = Outcome::Unverified;
        report.detail = Some(format!(
            "protected file(s) modified: {}; verification evidence is untrustworthy",
            trips.protected.join(", ")
        ));
        finalize(
            ctx,
            order,
            &task_id,
            &worktree,
            report,
            Some("summoner: protected verification config modified"),
        );
        return Ok(());
    }

    // Verification, finish-driven: run the order's profile, gate through the
    // independent reviewer while the task is still live, then attempt finish
    // and run exactly what a refusal names before one retry.
    let verify_started = Instant::now();
    let mut ran = std::collections::BTreeSet::new();
    let verified = profile_verify(ctx, order, &task_id, &worktree, report, &mut ran)?;
    report.timing.verify_secs = verify_started.elapsed().as_secs();
    if !verified {
        let abandon = match report.outcome {
            Outcome::Interrupted => Some("summoner: interrupted by operator"),
            _ => Some("summoner: verification failed"),
        };
        finalize(ctx, order, &task_id, &worktree, report, abandon);
        return Ok(());
    }

    let decision = match order.reviewer_name(ctx.config) {
        Some(reviewer) => Some(review_gate(
            ctx,
            order,
            &reviewer,
            &task_id,
            &worktree,
            &git_common_dir,
            &order_dir,
            &base,
            report,
        )?),
        None => None,
    };
    if matches!(decision, Some(ReviewDecision::Interrupted)) {
        report.outcome = Outcome::Interrupted;
        report.detail = Some("interrupted during review".into());
        finalize(
            ctx,
            order,
            &task_id,
            &worktree,
            report,
            Some("summoner: interrupted by operator"),
        );
        return Ok(());
    }

    finish_task(ctx, order, &task_id, &worktree, report, &mut ran)?;
    // The gate only ever narrows a success; a verification failure at finish
    // outranks whatever the reviewer thought.
    if matches!(report.outcome, Outcome::Verified | Outcome::Completed) {
        match decision {
            Some(ReviewDecision::Approve) if report.outcome == Outcome::Verified => {
                report.outcome = Outcome::Approved;
            }
            // Completed stays completed: approval cannot substitute for the
            // verification the repository never required.
            Some(ReviewDecision::Approve) | None => {}
            Some(ReviewDecision::Reject) => {
                report.outcome = Outcome::Rejected;
                report.detail = Some("review rejected; see review findings".into());
            }
            Some(ReviewDecision::Failed(reason)) => {
                report.outcome = Outcome::ReviewFailed;
                report.detail = Some(reason);
            }
            Some(ReviewDecision::Interrupted) => unreachable!("handled before finish"),
        }
    }

    let abandon = match report.outcome {
        // Review outcomes land after a successful finish: the task is
        // terminal, only the gate's judgment differs.
        Outcome::Verified
        | Outcome::Completed
        | Outcome::Approved
        | Outcome::Rejected
        | Outcome::ReviewFailed => None,
        Outcome::ScopeViolation => Some("summoner: writes outside declared scope"),
        Outcome::Interrupted => Some("summoner: interrupted by operator"),
        _ => Some("summoner: verification failed"),
    };
    finalize(ctx, order, &task_id, &worktree, report, abandon);
    Ok(())
}

/// The order's own verification profile. Returns false when the outcome is
/// already decided (profile failed or the run was interrupted).
fn profile_verify(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    ran: &mut std::collections::BTreeSet<String>,
) -> Result<bool> {
    // An interrupt cannot stop a verify subprocess mid-flight, but it must not
    // start the next one; the convergent tail still abandons and releases.
    if SHUTDOWN.load(Ordering::SeqCst) {
        report.outcome = Outcome::Interrupted;
        report.detail = Some("interrupted before verification".into());
        return Ok(false);
    }
    if let Some(profile) = order
        .verify_profile
        .clone()
        .or_else(|| ctx.config.default_verify_profile.clone())
    {
        let summary = ctx.grove.verify(worktree, &profile, task_id)?;
        let passed = summary.passed;
        ctx.events.emit(
            "order_verify",
            serde_json::json!({"id": order.id, "profile": profile, "passed": passed}),
        );
        ran.insert(profile.clone());
        report.verify.push(summary);
        if !passed {
            report.outcome = Outcome::Unverified;
            report.detail = Some(format!("verify profile {profile:?} failed"));
            return Ok(false);
        }
    }
    Ok(true)
}

/// Finish, refusal-driven: attempt it, run exactly the profiles a refusal
/// names, retry once.
fn finish_task(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    ran: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let _ = order;
    for attempt in 0..2 {
        if SHUTDOWN.load(Ordering::SeqCst) {
            report.outcome = Outcome::Interrupted;
            report.detail = Some("interrupted during verification".into());
            return Ok(());
        }
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
                // No verification block means grove refused for a reason this
                // version cannot act on; "the repository requires nothing" is
                // only ever an EXPLICIT empty required list.
                let Some(verification) = verification else {
                    report.outcome = Outcome::Unverified;
                    report.detail = Some(format!(
                        "finish refused ({reason}) without verification detail"
                    ));
                    return Ok(());
                };
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

enum ReviewDecision {
    Approve,
    Reject,
    Failed(String),
    Interrupted,
}

/// The independent quality gate: a fresh reviewer backend spawned under the
/// order's live task, given the requirements and the diff — never the
/// implementer's transcript. Any write it makes is undone and voids its
/// verdict, so an approve can only come from a reviewer that stayed read-only.
#[allow(clippy::too_many_arguments)]
fn review_gate(
    ctx: &Ctx,
    order: &Order,
    reviewer: &str,
    task_id: &str,
    worktree: &Path,
    git_common_dir: &Path,
    order_dir: &Path,
    base: &str,
    report: &mut OrderReport,
) -> Result<ReviewDecision> {
    let backend = &ctx.config.executors[reviewer];
    let timeout_secs = backend
        .timeout_secs
        .unwrap_or_else(|| ctx.config.order_timeout_secs());
    // The live delta, not base..HEAD: verification ran against this tree, so
    // the reviewer must judge everything in it — staged, unstaged, and (via
    // the status listing) untracked. A diff the gate cannot collect is an
    // error, never an empty diff silently approved.
    let diff = git(worktree, &["diff", base]).context("collecting the review diff")?;
    let diff_stat =
        git(worktree, &["diff", "--stat", base]).context("collecting the review diff stat")?;
    let uncommitted = git(worktree, &["status", "--porcelain"])
        .context("collecting the review status listing")?;
    let prompt = review::compose_prompt(
        order,
        base,
        &report.tripwires,
        &report.verify,
        &diff,
        &diff_stat,
        &uncommitted,
    );
    let before = review::snapshot(worktree)?;
    let stdout_log = order_dir.join("review-stdout.log");
    // Reviews run for minutes; a live consumer needs the logs to tail the
    // moment the reviewer spawns, not a verdict event after the fact.
    ctx.events.emit(
        "review_started",
        serde_json::json!({
            "id": order.id,
            "reviewer": reviewer,
            "stdout_log": stdout_log.display().to_string(),
            "stderr_log": order_dir.join("review-stderr.log").display().to_string(),
            "timeout_secs": timeout_secs,
        }),
    );
    let started = Instant::now();
    let exec = executor::run_executor(&ExecRequest {
        grove: &ctx.grove,
        backend,
        order,
        task_id,
        worktree,
        git_common_dir,
        run_dir: order_dir,
        timeout_secs,
        shutdown: &SHUTDOWN,
        prompt: &prompt,
        file_prefix: "review-",
    })?;
    let mut summary = ReviewSummary {
        reviewer: reviewer.to_string(),
        verdict: "failed".into(),
        detail: None,
        findings: Vec::new(),
        exit: exec.exit,
        duration_secs: started.elapsed().as_secs(),
        stdout_log: Some(stdout_log.display().to_string()),
        stderr_log: Some(order_dir.join("review-stderr.log").display().to_string()),
    };
    if let Some(marker) = &backend.usage_marker
        && let Some(extra) = ["review-stderr.log", "review-stdout.log"]
            .iter()
            .find_map(|name| {
                executor::tail(&order_dir.join(name), 8192)
                    .as_deref()
                    .and_then(|text| number_after(text, marker))
            })
    {
        report.usage_tokens = Some(report.usage_tokens.unwrap_or(0).saturating_add(extra));
    }
    // A wedged supervisor can leave the reviewer's group alive and still
    // writing; kill it BEFORE undoing worktree state, or the restoration
    // races the very process it is cleaning up after.
    if exec.backup_killed {
        kill_recorded_group(ctx, task_id, worktree);
    }
    let violations = review::restore(worktree, &before)?;

    let decision = if exec.backup_killed {
        summary.detail = Some("review supervisor did not return; backup deadline fired".into());
        ReviewDecision::Failed("review failed: supervisor wedged".into())
    } else if SHUTDOWN.load(Ordering::SeqCst) {
        summary.detail = Some("interrupted by operator".into());
        ReviewDecision::Interrupted
    } else if !violations.is_empty() {
        summary.detail = Some(format!(
            "reviewer modified the worktree (undone): {}",
            violations.join(", ")
        ));
        ReviewDecision::Failed("review failed: reviewer modified the worktree".into())
    } else if exec.exit == Some(124) {
        summary.detail = Some("review timed out".into());
        ReviewDecision::Failed("review failed: timeout".into())
    } else if exec.exit != Some(0) {
        summary.detail = Some(format!("reviewer exited {:?}", exec.exit));
        ReviewDecision::Failed(format!("review failed: reviewer exited {:?}", exec.exit))
    } else {
        match executor::tail(&stdout_log, 64 * 1024)
            .as_deref()
            .and_then(review::parse_verdict)
        {
            Some(parsed) => {
                summary.findings = parsed.findings;
                match parsed.verdict {
                    review::Verdict::Approve => {
                        summary.verdict = "approve".into();
                        ReviewDecision::Approve
                    }
                    review::Verdict::Reject => {
                        summary.verdict = "reject".into();
                        ReviewDecision::Reject
                    }
                }
            }
            None => {
                summary.detail = Some("no verdict JSON in reviewer output".into());
                ReviewDecision::Failed("review failed: no verdict in output".into())
            }
        }
    };
    ctx.events.emit(
        "order_review",
        serde_json::json!({
            "id": order.id,
            "reviewer": reviewer,
            "verdict": summary.verdict,
            "findings": summary.findings.len(),
            "detail": summary.detail,
        }),
    );
    report.review = Some(summary);
    Ok(decision)
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
        && !matches!(
            report.outcome,
            Outcome::Verified | Outcome::Completed | Outcome::Approved
        );
    if keep {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; worktree kept for post-mortem"),
            None => "worktree kept for post-mortem".to_string(),
        });
    } else {
        release(ctx, worktree, report);
        // A leaked worktree (or failed salvage) is not a success, whatever the
        // receipts say: dependents must not build on it and the run must not
        // exit 0. Deliberate keep_failed_worktrees is different — that skip is
        // requested, not a failure.
        if report.release_error.is_some()
            && matches!(
                report.outcome,
                Outcome::Verified | Outcome::Completed | Outcome::Approved
            )
        {
            report.outcome = Outcome::Error;
        }
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

/// The dependency-aware queue. An order is ready when every `after` id reached
/// a successful outcome; a dependency that landed anywhere else condemns its
/// dependents to `skipped`. Cycles and unknown ids were rejected in validation,
/// and a dependency still in `pending` is always scanned before its dependent,
/// so `Wait` can only mean work is genuinely in flight.
struct Scheduler {
    pending: Vec<Order>,
    done: BTreeMap<String, Outcome>,
    /// Circuit breaker: after this many failures, the rest of the queue is
    /// skipped instead of spending executor budget on a doomed fleet.
    fail_fast: Option<usize>,
    failures: usize,
}

enum Next {
    Run(Box<Order>),
    Skip(Box<Order>, String),
    Wait,
    Done,
}

impl Scheduler {
    fn new(orders: Vec<Order>, fail_fast: Option<usize>) -> Self {
        Scheduler {
            pending: orders,
            done: BTreeMap::new(),
            fail_fast,
            failures: 0,
        }
    }

    fn next(&mut self) -> Next {
        if self.pending.is_empty() {
            return Next::Done;
        }
        if let Some(limit) = self.fail_fast
            && self.failures >= limit
        {
            let order = self.pending.remove(0);
            return Next::Skip(
                Box::new(order),
                format!(
                    "not started: fail_fast tripped after {} failure(s)",
                    self.failures
                ),
            );
        }
        for index in 0..self.pending.len() {
            let mut in_flight = false;
            let mut condemned = None;
            for dep in &self.pending[index].after {
                match self.done.get(dep) {
                    Some(Outcome::Verified | Outcome::Completed | Outcome::Approved) => {}
                    Some(outcome) => {
                        condemned = Some(format!("dependency {dep:?} was {}", outcome.key()));
                        break;
                    }
                    None => {
                        in_flight = true;
                        break;
                    }
                }
            }
            if let Some(reason) = condemned {
                return Next::Skip(Box::new(self.pending.remove(index)), reason);
            }
            if !in_flight {
                return Next::Run(Box::new(self.pending.remove(index)));
            }
        }
        Next::Wait
    }

    fn drain(&mut self) -> Next {
        match self.pending.pop() {
            Some(order) => Next::Skip(Box::new(order), "not started: run interrupted".into()),
            None => Next::Done,
        }
    }

    fn complete(&mut self, id: &str, outcome: Outcome) {
        // Coordination artifacts (blocked) and operator actions (interrupted,
        // skipped) are not executor failures; they must not trip the breaker.
        if matches!(
            outcome,
            Outcome::Error
                | Outcome::Stalled
                | Outcome::ExecutorFailed
                | Outcome::ScopeViolation
                | Outcome::Unverified
                | Outcome::ReviewFailed
                | Outcome::Rejected
        ) {
            self.failures += 1;
        }
        self.done.insert(id.to_string(), outcome);
    }
}

fn skipped(order: &Order, config: &Config, reason: String) -> OrderReport {
    let executor = order.executor_name(config).unwrap_or_default();
    let mut report = OrderReport::new(order, executor);
    report.outcome = Outcome::Skipped;
    report.detail = Some(reason);
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

/// The first number after the LAST occurrence of `marker`, tolerating comma
/// and underscore separators (codex prints "tokens used\n40,958").
fn number_after(text: &str, marker: &str) -> Option<u64> {
    let rest = &text[text.rfind(marker)? + marker.len()..];
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_')
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
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
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("summoner")
        .join("runs")
}

#[cfg(unix)]
fn kill_recorded_group(ctx: &Ctx, task_id: &str, worktree: &Path) {
    let Ok(status) = ctx.grove.task_status(worktree) else {
        return;
    };
    let Some(tasks) = status["tasks"].as_array() else {
        return;
    };
    for task in tasks {
        if task["id"] == task_id
            && let Some(pid) = task["active_command"]["pid"].as_u64()
        {
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_recorded_group(_ctx: &Ctx, _task_id: &str, _worktree: &Path) {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_numbers_parse_after_the_last_marker() {
        assert_eq!(
            number_after("...\ntokens used\n40,958\n", "tokens used"),
            Some(40_958)
        );
        assert_eq!(
            number_after("tokens used: 5\nmore\ntokens used: 1_200", "tokens used"),
            Some(1_200)
        );
        assert_eq!(number_after("no marker here", "tokens used"), None);
        assert_eq!(
            number_after("tokens used but no number", "tokens used"),
            None
        );
    }
}
