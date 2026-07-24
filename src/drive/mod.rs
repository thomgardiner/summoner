//! The per-order state machine: acquire a worktree, begin the task, then run
//! the attempt loop — exec, tripwires, verify, review, finish.

mod attempt;
mod revise;

use crate::order::Order;
use crate::outcome::{finalize, release};
use crate::report::{OrderReport, Outcome, WorkerFailure};
use crate::run::Ctx;
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

use attempt::OrderRun;

pub(crate) fn run_order(ctx: &Ctx, order: &Order) -> OrderReport {
    let executor_name = order
        .executor_name(ctx.config)
        .expect("validated before dispatch");
    let mut report = OrderReport::new(order, executor_name.clone());
    if let Some(prior) = ctx.prior.iter().find(|prior| prior.id == order.id) {
        report.attempts = prior.attempts.saturating_add(1);
        report.session_id = prior.session_id.clone();
        report.usage_tokens = prior.usage_tokens;
    }
    let total = Instant::now();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drive(ctx, order, &executor_name, &mut report)
    })) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => fail(ctx, order, &mut report, format!("{error:#}"), None),
        Err(payload) => {
            let failure = WorkerFailure::panic(payload);
            let detail = format!("worker panicked: {}", failure.message);
            fail(ctx, order, &mut report, detail, Some(failure));
        }
    }
    report.timing.total_secs = total.elapsed().as_secs();
    report
}

pub(crate) fn fail(
    ctx: &Ctx<'_>,
    order: &Order,
    report: &mut OrderReport,
    detail: String,
    failure: Option<WorkerFailure>,
) {
    report.outcome = Outcome::Error;
    report.detail = Some(detail);
    report.worker_failure = failure;
    let abandon = report
        .worker_failure
        .as_ref()
        .map_or("summoner: internal error", |_| "summoner: worker panicked");
    match (report.task_id.clone(), report.worktree.clone()) {
        (Some(task_id), Some(worktree)) => finalize(
            ctx,
            order,
            &task_id,
            Path::new(&worktree),
            report,
            Some(abandon),
        ),
        (None, Some(worktree)) => release(ctx, Path::new(&worktree), report),
        _ => {}
    }
}

/// Sets `report.outcome` on every path; returns Err only for summoner-side
/// failures that map to `error`.
pub(crate) fn drive(
    ctx: &Ctx,
    order: &Order,
    executor_name: &str,
    report: &mut OrderReport,
) -> Result<()> {
    ctx.events.emit(
        "order_started",
        serde_json::json!({"id": order.id, "executor": executor_name}),
    )?;
    let Some(mut run) = OrderRun::begin(ctx, order, executor_name, report)? else {
        return Ok(());
    };
    loop {
        if let Flow::Done = run.attempt(report)? {
            return Ok(());
        }
    }
}

pub(crate) enum Flow {
    /// The order reached a terminal outcome; finalize already ran.
    Done,
    /// A revision was scheduled; run another attempt.
    Retry,
}
