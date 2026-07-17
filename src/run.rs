//! The fleet loop: validated orders in, one ranked report out. Every order
//! walks the same state machine, and every arm converges on the same tail —
//! collect evidence, terminalize the grove task, release the worktree, report.

use crate::config::Config;
use crate::events::EventSink;
use crate::grove::GroveCli;
use crate::order::{self, Order};
use crate::report::{OrderReport, Outcome, RunReport};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// One panicked worker must not poison the whole fleet: the guarded data
/// (queue, report list) keeps its invariants per-operation, so recover the
/// lock and keep collecting the other orders' reports.
fn relock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct Ctx<'a> {
    pub(crate) config: &'a Config,
    pub(crate) grove: GroveCli,
    pub(crate) repo: PathBuf,
    pub(crate) run_dir: PathBuf,
    pub(crate) events: EventSink,
    /// Tokens recorded so far across ALL workers and attempts, updated the
    /// moment usage is scraped — the budget breaker and the revision loop
    /// both read it, so an in-flight fleet reacts to spend, not just the
    /// between-orders bookkeeping.
    pub(crate) spent: AtomicU64,
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
            // Carry the prior evidence that still matters: spend counts
            // against this run's budget, attempts and the session id let the
            // orchestrator keep working with what already happened.
            if let Some(prior) = prior_orders
                .iter()
                .find(|entry| entry["id"].as_str() == Some(order.id.as_str()))
            {
                report.usage_tokens = prior["usage_tokens"].as_u64();
                report.attempts = prior["attempts"].as_u64().unwrap_or(1);
                report.session_id = prior["session_id"].as_str().map(String::from);
                report.saved_to = prior["saved_to"].as_str().map(String::from);
            }
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
        spent: AtomicU64::new(
            carried
                .iter()
                .filter_map(|prior| prior.usage_tokens)
                .fold(0, u64::saturating_add),
        ),
    };
    let started = Instant::now();
    let mut scheduler = Scheduler::new(orders, config.fail_fast(), config.run_token_budget());
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
                            scheduler.next(ctx.spent.load(Ordering::SeqCst))
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
                            let report = crate::drive::run_order(&ctx, &order);
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
                            "attempts": report.attempts,
                            "session_id": report.session_id,
                            "branch": report.branch,
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
    crate::scorecard::record(&runs_root(), &report);
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
    /// Token ceiling for the whole run; once live spend crosses it, the rest
    /// of the queue is skipped (and the revision loop stops revising).
    budget: Option<u64>,
}

enum Next {
    Run(Box<Order>),
    Skip(Box<Order>, String),
    Wait,
    Done,
}

impl Scheduler {
    fn new(orders: Vec<Order>, fail_fast: Option<usize>, budget: Option<u64>) -> Self {
        Scheduler {
            pending: orders,
            done: BTreeMap::new(),
            fail_fast,
            failures: 0,
            budget,
        }
    }

    fn next(&mut self, spent: u64) -> Next {
        if self.pending.is_empty() {
            return Next::Done;
        }
        if let Some(budget) = self.budget
            && spent >= budget
        {
            let order = self.pending.remove(0);
            return Next::Skip(
                Box::new(order),
                format!(
                    "not started: run token budget exhausted ({spent} of {budget} tokens spent)"
                ),
            );
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

pub(crate) fn runs_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: &str) -> Order {
        Order {
            id: id.into(),
            title: "t".into(),
            brief: "b".into(),
            scope: vec!["src".into()],
            acceptance: Vec::new(),
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            max_tokens: None,
            base: None,
            branch: None,
            variants: Vec::new(),
            claim_group: None,
            variant_of: None,
            after: Vec::new(),
            source: PathBuf::from(format!("{id}.toml")),
        }
    }

    #[test]
    fn budget_breaker_skips_the_queue_once_spent_crosses_the_ceiling() {
        let mut scheduler = Scheduler::new(vec![order("a"), order("b")], None, Some(100));
        let Next::Run(first) = scheduler.next(0) else {
            panic!("first order dispatches under budget");
        };
        scheduler.complete(&first.id, Outcome::Verified);
        // Live spend is what the breaker reads, not completion bookkeeping.
        match scheduler.next(150) {
            Next::Skip(second, reason) => {
                assert_eq!(second.id, "b");
                assert!(
                    reason.contains("budget exhausted (150 of 100 tokens spent)"),
                    "{reason}"
                );
            }
            _ => panic!("over-budget queue must drain as skipped"),
        }
    }
}
