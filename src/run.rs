//! Fleet scheduling and durable report publication.

use crate::config::Config;
use crate::events::EventSink;
use crate::grove::GroveCli;
use crate::order::Order;
use crate::report::{OrderReport, Outcome, RunReport};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) static SHUTDOWN: AtomicBool = AtomicBool::new(false);
/// Recover a poisoned scheduler lock; its invariants are per-operation.
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
    pub(crate) prior: &'a [OrderReport],
    /// Live spend across all workers and attempts.
    pub(crate) spent: AtomicU64,
}
pub fn run(
    config: &Config,
    selected: Option<&str>,
    paths: &[PathBuf],
    stream: bool,
) -> Result<i32> {
    crate::config::selected_profile(selected);
    let grove = GroveCli::new(config.grove_bin());
    grove.preflight()?;
    let orders = crate::run_prepare::validated(paths, config)?;
    execute(config, grove, orders, stream, Vec::new(), Vec::new())
}
pub fn resume(config: &Config, _selected: Option<&str>, run_id: &str, stream: bool) -> Result<i32> {
    crate::run_resume::resume(config, run_id, stream)
}
pub(crate) fn execute(
    config: &Config,
    grove: GroveCli,
    orders: Vec<Order>,
    stream: bool,
    carried: Vec<OrderReport>,
    prior: Vec<OrderReport>,
) -> Result<i32> {
    let grove_version = grove.version()?;
    let selected_profile = crate::config::profile();
    let repo = std::env::current_dir()
        .context("resolving current directory")?
        .canonicalize()
        .context("canonicalizing repository path")?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_id = format!("{started_at}-{}", std::process::id());
    let run_dir = runs_root().join(&run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("creating run dir {}", run_dir.display()))?;

    crate::run_evidence::write_manifest(
        &run_dir,
        &run_id,
        &repo,
        selected_profile.as_deref(),
        &grove_version,
        config,
        &orders,
    )?;
    let carried_ids: BTreeSet<&str> = carried.iter().map(|report| report.id.as_str()).collect();
    let orders: Vec<Order> = orders
        .into_iter()
        .filter(|order| !carried_ids.contains(order.id.as_str()))
        .collect();

    install_interrupt_handler();

    let events = EventSink::new(&run_dir, run_id.clone(), stream)?;
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
    )?;
    let ctx = Ctx {
        config,
        grove,
        repo: repo.clone(),
        run_dir: run_dir.clone(),
        events,
        prior: &prior,
        spent: AtomicU64::new(
            carried
                .iter()
                .chain(&prior)
                .filter_map(|prior| prior.usage_tokens)
                .fold(0, u64::saturating_add),
        ),
    };
    let started = Instant::now();
    let mut scheduler = Scheduler::new(orders, config.fail_fast(), config.run_token_budget());
    // Carried orders count as done so their dependents dispatch immediately, and
    // each is a durable terminal record before any worker dispatches.
    for prior in &carried {
        scheduler.complete(&prior.id, prior.outcome);
        ctx.events.emit_terminal("order_carried", prior)?;
    }
    let scheduler = Mutex::new(scheduler);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = {
                        let mut scheduler = relock(&scheduler);
                        // A failed journal stops dispatch: drain instead of
                        // launching more work that could never be recorded.
                        if SHUTDOWN.load(Ordering::SeqCst) || ctx.events.failed() {
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
                    // The terminal transition is the durable record report.json
                    // is projected from; a journal failure here halts dispatch.
                    if ctx.events.emit_terminal("order_finished", &report).is_err() {
                        break;
                    }
                    relock(&scheduler).complete(&order.id, report.outcome);
                }
            });
        }
    });

    // A dispatch journal failure is fatal: never rank from unrecorded memory.
    ctx.events.check()?;
    let orders = crate::run_journal::terminal_reports(&run_dir.join("events.jsonl"), &run_id)
        .context("projecting order reports from the run journal")?;
    let report = RunReport::assemble(
        run_id,
        repo.display().to_string(),
        started_at,
        started.elapsed().as_secs(),
        orders,
    );
    // Fail closed: record run_finished before publishing report.json or the scorecard.
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
    )?;
    crate::run_evidence::write_once(&run_dir.join("report.json"), &report)
        .context("writing report.json")?;
    crate::scorecard::record(&runs_root(), &report);
    if ctx.events.streaming() {
        // Stream consumers get the complete report as the final NDJSON line;
        // the pretty print would break line-oriented parsers.
        println!(
            "{}",
            serde_json::json!({"event": "report", "report": &report})
        );
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(report.exit_code())
}

/// Dependency queue; validation already rejected cycles and unknown ids.
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
