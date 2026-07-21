//! Dependency scheduling and worker threads for one fleet run.

use super::{Ctx, SHUTDOWN};
use crate::config::Config;
use crate::order::Order;
use crate::report::{OrderReport, Outcome, WorkerFailure};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(super) struct Fleet {
    scheduler: Mutex<Scheduler>,
}

impl Fleet {
    pub(super) fn new(orders: Vec<Order>, fail_fast: Option<usize>, budget: Option<u64>) -> Self {
        Self {
            scheduler: Mutex::new(Scheduler::new(orders, fail_fast, budget)),
        }
    }

    pub(super) fn carry(&mut self, id: &str, outcome: Outcome) {
        let scheduler = match self.scheduler.get_mut() {
            Ok(scheduler) => scheduler,
            Err(error) => {
                let scheduler = error.into_inner();
                scheduler.poisoned = true;
                scheduler
            }
        };
        scheduler.complete(id, outcome);
    }

    pub(super) fn run(&self, ctx: &Ctx<'_>, workers: usize) -> Result<()> {
        self.dispatch(ctx, workers, crate::drive::run_order)
    }

    pub(super) fn dispatch<F>(&self, ctx: &Ctx<'_>, workers: usize, run_order: F) -> Result<()>
    where
        F: Fn(&Ctx<'_>, &Order) -> OrderReport + Sync,
    {
        let coordinator_failed = AtomicBool::new(false);
        let failures = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for _ in 0..workers {
                handles.push(scope.spawn(|| {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        self.worker(ctx, &run_order, &coordinator_failed);
                    }));
                    if result.is_err() {
                        coordinator_failed.store(true, Ordering::SeqCst);
                    }
                    result
                }));
            }
            handles
                .into_iter()
                .filter_map(|handle| match handle.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(payload)) | Err(payload) => Some(WorkerFailure::panic(payload)),
                })
                .collect::<Vec<_>>()
        });
        if failures.is_empty() {
            Ok(())
        } else {
            let messages = failures
                .iter()
                .map(|failure| failure.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "{} scheduler worker(s) panicked outside an order boundary: {messages}",
                failures.len()
            )
        }
    }

    fn worker<F>(&self, ctx: &Ctx<'_>, run_order: &F, coordinator_failed: &AtomicBool)
    where
        F: Fn(&Ctx<'_>, &Order) -> OrderReport,
    {
        loop {
            let next = {
                let mut scheduler = self.lock();
                if SHUTDOWN.load(Ordering::SeqCst)
                    || coordinator_failed.load(Ordering::SeqCst)
                    || ctx.events.failed()
                {
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
                    let report = match catch_unwind(AssertUnwindSafe(|| run_order(ctx, &order))) {
                        Ok(report) => report,
                        Err(payload) => failed(&order, ctx.config, WorkerFailure::panic(payload)),
                    };
                    (order, report)
                }
                Next::Fail(order, failure) => {
                    let report = failed(&order, ctx.config, failure);
                    (order, report)
                }
            };
            if ctx.events.emit_terminal("order_finished", &report).is_err() {
                break;
            }
            self.lock().complete(&order.id, report.outcome);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scheduler> {
        match self.scheduler.lock() {
            Ok(scheduler) => scheduler,
            Err(error) => {
                let mut scheduler = error.into_inner();
                scheduler.poisoned = true;
                self.scheduler.clear_poison();
                scheduler
            }
        }
    }

    #[cfg(test)]
    pub(super) fn poison(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _scheduler = self.scheduler.lock().expect("scheduler starts clean");
                panic!("poison scheduler");
            });
            assert!(handle.join().is_err());
        });
    }
}

/// Dependency queue; validation already rejected cycles and unknown ids.
struct Scheduler {
    pending: Vec<Order>,
    done: BTreeMap<String, Outcome>,
    fail_fast: Option<usize>,
    failures: usize,
    budget: Option<u64>,
    poisoned: bool,
}

enum Next {
    Run(Box<Order>),
    Skip(Box<Order>, String),
    Fail(Box<Order>, WorkerFailure),
    Wait,
    Done,
}

impl Scheduler {
    fn new(orders: Vec<Order>, fail_fast: Option<usize>, budget: Option<u64>) -> Self {
        Self {
            pending: orders,
            done: BTreeMap::new(),
            fail_fast,
            failures: 0,
            budget,
            poisoned: false,
        }
    }

    fn next(&mut self, spent: u64) -> Next {
        if self.pending.is_empty() {
            return Next::Done;
        }
        if self.poisoned {
            self.poisoned = false;
            return Next::Fail(Box::new(self.pending.remove(0)), WorkerFailure::poisoned());
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
                    None => in_flight = true,
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

fn failed(order: &Order, config: &Config, failure: WorkerFailure) -> OrderReport {
    let executor = order.executor_name(config).unwrap_or_default();
    let mut report = OrderReport::new(order, executor);
    report.outcome = Outcome::Error;
    report.detail = Some(failure.message.clone());
    report.worker_failure = Some(failure);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn order(id: &str) -> Order {
        Order {
            id: id.into(),
            title: "t".into(),
            brief: "b".into(),
            scope: vec!["src".into()],
            acceptance: Vec::new(),
            verify_profile: None,
            executor: Some("fake".into()),
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
        match scheduler.next(150) {
            Next::Skip(second, reason) => {
                assert_eq!(second.id, "b");
                assert!(reason.contains("budget exhausted (150 of 100 tokens spent)"));
            }
            _ => panic!("over-budget queue must drain as skipped"),
        }
    }
}
