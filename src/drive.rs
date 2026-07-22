//! The per-order state machine: acquire a worktree, begin the task, then run
//! the attempt loop — exec, tripwires, verify, review, finish. A rejected or
//! unverified attempt re-dispatches with its failure evidence up to `revise`
//! extra times; every other outcome exits on the first pass. Every arm
//! converges on `outcome::finalize`, so claims and worktrees never leak.

use crate::executor::{self, ExecOutcome, ExecRequest};
use crate::gate::{ReviewDecision, finish_task, profile_verify, review_gate};
use crate::grove::BeginOutcome;
use crate::order::Order;
use crate::outcome::{
    finalize, git, head_and_tail, kill_recorded_group, number_after, release, token_after,
};
use crate::report::{OrderReport, Outcome, WorkerFailure};
use crate::run::{Ctx, SHUTDOWN};
use crate::tripwires;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

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

fn fail(
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
fn drive(ctx: &Ctx, order: &Order, executor_name: &str, report: &mut OrderReport) -> Result<()> {
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

enum Flow {
    /// The order reached a terminal outcome; finalize already ran.
    Done,
    /// A revision was scheduled; run another attempt.
    Retry,
}

struct OrderRun<'a> {
    ctx: &'a Ctx<'a>,
    order: &'a Order,
    backend: &'a crate::config::ExecutorBackend,
    agent: String,
    worktree: PathBuf,
    git_common_dir: PathBuf,
    order_dir: PathBuf,
    base: String,
    timeout_secs: u64,
    task_id: String,
    max_attempts: u64,
    feedback: String,
}

impl<'a> OrderRun<'a> {
    /// Acquire the worktree and begin the task. `None` means the order is
    /// already terminal (blocked on a claim conflict).
    fn begin(
        ctx: &'a Ctx<'a>,
        order: &'a Order,
        executor_name: &str,
        report: &mut OrderReport,
    ) -> Result<Option<Self>> {
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
                return Ok(None);
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

        // No --path-format=absolute: that flag needs git >= 2.31, and
        // absolutizing a relative answer against the worktree is version-proof.
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
        )?;
        let base = report.base_commit.clone().unwrap_or_else(|| "HEAD".into());
        Ok(Some(OrderRun {
            ctx,
            order,
            backend,
            agent,
            worktree,
            git_common_dir,
            order_dir,
            base,
            timeout_secs,
            task_id,
            max_attempts: report.attempts + ctx.config.revise() as u64,
            feedback: String::new(),
        }))
    }

    /// One full attempt. Terminal outcomes finalize and return `Done`;
    /// a scheduled revision returns `Retry`.
    fn attempt(&mut self, report: &mut OrderReport) -> Result<Flow> {
        let attempt = report.attempts;
        let prefix = if attempt == 1 {
            String::new()
        } else {
            format!("r{attempt}-")
        };
        // Tripwires reflect this attempt only. Clearing here, before anything
        // adds to them, lets scrape_output's warnings survive the diff-scan
        // assignment below instead of being overwritten by it.
        report.tripwires.clear();
        let exec = self.spawn_executor(report, attempt, &prefix)?;
        self.scrape_output(report, &prefix);
        self.ctx.events.emit(
            "order_exec_done",
            serde_json::json!({
                "id": self.order.id,
                "attempt": attempt,
                "exit": exec.exit,
                "backup_killed": exec.backup_killed,
                "usage_tokens": report.usage_tokens,
                "session_id": report.session_id,
            }),
        )?;
        if let Some((outcome, reason)) = self.classify_exec(report, &exec) {
            report.outcome = outcome;
            return self.done(report, Some(reason));
        }
        if !self.order.acceptance.is_empty() {
            let result = self.worker_result(&prefix);
            if let Err(detail) = result.unwrap_or_else(|| {
                Err("executor omitted the required summoner completion result".into())
            }) {
                report.outcome = Outcome::Unverified;
                report.detail = Some(detail);
                if self.try_revise(report)? {
                    return Ok(Flow::Retry);
                }
                return self.done(report, Some("summoner: executor reported incomplete work"));
            }
        }
        if self.protected_tripwire(report)? {
            return self.done(
                report,
                Some("summoner: protected verification config modified"),
            );
        }
        if !self.work_changed()? {
            report.outcome = Outcome::Unverified;
            report.detail = Some(
                "executor produced no changes; baseline verification cannot prove acceptance"
                    .into(),
            );
            if self.try_revise(report)? {
                return Ok(Flow::Retry);
            }
            return self.done(report, Some("summoner: executor produced no changes"));
        }
        self.judge(report, &prefix)
    }

    /// Verification, review, finish, and the post-finish revision decision.
    fn judge(&mut self, report: &mut OrderReport, prefix: &str) -> Result<Flow> {
        let verify_started = Instant::now();
        let mut ran = std::collections::BTreeSet::new();
        let verified = profile_verify(
            self.ctx,
            self.order,
            &self.task_id,
            &self.worktree,
            report,
            &mut ran,
        )?;
        report.timing.verify_secs += verify_started.elapsed().as_secs();
        if !verified {
            // Verification failed before finish, so the task is still live
            // and its claims are still this order's: re-exec only.
            if report.outcome == Outcome::Unverified && self.try_revise(report)? {
                return Ok(Flow::Retry);
            }
            let abandon = match report.outcome {
                Outcome::Interrupted => "summoner: interrupted by operator",
                _ => "summoner: verification failed",
            };
            return self.done(report, Some(abandon));
        }

        let decision = match self.order.reviewer_name(self.ctx.config) {
            Some(reviewer) => Some(review_gate(
                self.ctx,
                self.order,
                &reviewer,
                &self.task_id,
                &self.worktree,
                &self.git_common_dir,
                &self.order_dir,
                &self.base,
                prefix,
                report,
            )?),
            None => None,
        };
        if matches!(decision, Some(ReviewDecision::Interrupted)) {
            report.outcome = Outcome::Interrupted;
            report.detail = Some("interrupted during review".into());
            return self.done(report, Some("summoner: interrupted by operator"));
        }

        let expected_source = match decision.as_ref() {
            Some(ReviewDecision::Approve(source) | ReviewDecision::Reject(source)) => {
                Some(source.as_str())
            }
            _ => None,
        };
        finish_task(
            self.ctx,
            self.order,
            &self.task_id,
            &self.worktree,
            report,
            &mut ran,
            expected_source,
        )?;
        self.map_review(report, decision);

        // Finish succeeded on a rejection: the task is terminal and its
        // claims released, so a revision needs a fresh task. Finish refused
        // on evidence (unverified): the task is still active, re-exec only.
        if report.outcome == Outcome::Rejected && self.rebegin_for_revision(report)? {
            return Ok(Flow::Retry);
        }
        if report.outcome == Outcome::Unverified && self.try_revise(report)? {
            return Ok(Flow::Retry);
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
        self.done(report, abandon)
    }

    fn spawn_executor(
        &self,
        report: &mut OrderReport,
        attempt: u64,
        prefix: &str,
    ) -> Result<ExecOutcome> {
        report.stdout_log = Some(
            self.order_dir
                .join(format!("{prefix}stdout.log"))
                .display()
                .to_string(),
        );
        report.stderr_log = Some(
            self.order_dir
                .join(format!("{prefix}stderr.log"))
                .display()
                .to_string(),
        );
        // A revision resumes the executor's own session when the backend
        // supports it: the charter and order are already in context, so only
        // the evidence travels.
        let resumed = !self.backend.resume_argv.is_empty() && report.session_id.is_some();
        let prompt = if attempt == 1 {
            executor::compose_prompt(self.order)
        } else {
            executor::compose_revision_prompt(self.order, attempt, resumed, &self.feedback)
        };
        let template: &[String] = if resumed {
            &self.backend.resume_argv
        } else {
            &self.backend.argv
        };
        let exec_started = Instant::now();
        let exec = executor::run_executor(&ExecRequest {
            grove: &self.ctx.grove,
            backend: self.backend,
            order: self.order,
            task_id: &self.task_id,
            worktree: &self.worktree,
            git_common_dir: &self.git_common_dir,
            run_dir: &self.order_dir,
            timeout_secs: self.timeout_secs,
            shutdown: &SHUTDOWN,
            argv: template,
            resume: resumed,
            session_id: report.session_id.as_deref().unwrap_or(""),
            prompt: &prompt,
            file_prefix: prefix,
        })?;
        report.timing.exec_secs += exec_started.elapsed().as_secs();
        report.executor_exit = exec.exit;
        Ok(exec)
    }

    /// Token usage counts against the live run budget the moment it is
    /// known; a captured session id must be id-shaped or it is ignored.
    fn scrape_output(&self, report: &mut OrderReport, prefix: &str) {
        let logs = [format!("{prefix}stderr.log"), format!("{prefix}stdout.log")];
        if let Some(marker) = &self.backend.usage_marker {
            match logs.iter().find_map(|name| {
                executor::tail(&self.order_dir.join(name), 8192)
                    .as_deref()
                    .and_then(|text| number_after(text, marker))
            }) {
                Some(used) => {
                    report.usage_tokens =
                        Some(report.usage_tokens.unwrap_or(0).saturating_add(used));
                    self.ctx.spent.fetch_add(used, Ordering::SeqCst);
                }
                // A marker that never matches is a silent hole in budget
                // tracking (a vendor CLI whose banner changed, or one that
                // emits JSON): say so instead of reporting nothing at all.
                None => report.tripwires.push(format!(
                    "usage_marker {marker:?} never matched this attempt's output;                      token usage was not tracked"
                )),
            }
        }
        if let Some(marker) = &self.backend.session_marker
            && let Some(session) = logs.iter().find_map(|name| {
                head_and_tail(&self.order_dir.join(name))
                    .as_deref()
                    .and_then(|text| token_after(text, marker))
            })
        {
            report.session_id = Some(session);
        }
    }

    fn classify_exec(
        &self,
        report: &mut OrderReport,
        exec: &ExecOutcome,
    ) -> Option<(Outcome, &'static str)> {
        if exec.backup_killed {
            // grove itself was wedged, so its deadline never fired — but its
            // task record still names the executor process-group leader. Kill
            // that group directly: a stuck fleet must not keep spending
            // executor budget unsupervised.
            kill_recorded_group(self.ctx, &self.task_id, &self.worktree);
            return Some((
                Outcome::Stalled,
                "summoner: backup deadline fired; grove supervisor did not return",
            ));
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            return Some((Outcome::Interrupted, "summoner: interrupted by operator"));
        }
        match exec.exit {
            Some(124) => Some((Outcome::Stalled, "summoner: executor timeout")),
            Some(0) => None,
            code => {
                report.detail = Some(format!("executor exit {code:?}"));
                Some((Outcome::ExecutorFailed, "summoner: executor failed"))
            }
        }
    }

    fn worker_result(&self, prefix: &str) -> Option<Result<(), String>> {
        [format!("{prefix}stdout.log"), format!("{prefix}stderr.log")]
            .iter()
            .find_map(|name| {
                head_and_tail(&self.order_dir.join(name))
                    .as_deref()
                    .and_then(parse_worker_result)
            })
    }

    /// Deterministic anti-hacking scan before any evidence is trusted. A
    /// modified verification config invalidates the receipts it would
    /// produce, so that is a hard stop and never revised; soft flags ride
    /// along to the reviewer. A scan that cannot collect evidence propagates
    /// as `error`: the gate never reports a pass it did not perform.
    fn protected_tripwire(&self, report: &mut OrderReport) -> Result<bool> {
        let policy_protected = self
            .ctx
            .config
            .trusted_policy
            .as_ref()
            .map(|policy| policy.protected_paths.as_slice())
            .unwrap_or_default();
        let trips = tripwires::scan(&self.worktree, &self.base, policy_protected)?;
        // Extend, not assign: scrape_output may already have added a run-quality
        // warning (an unmatched usage_marker) that must not be wiped by the
        // diff scan. Tripwires are cleared once per attempt, so this never
        // accumulates across attempts.
        report.tripwires.extend(trips.flags.iter().cloned());
        if trips.protected.is_empty() {
            return Ok(false);
        }
        report.outcome = Outcome::Unverified;
        report.detail = Some(format!(
            "protected file(s) modified: {}; verification evidence is untrustworthy",
            trips.protected.join(", ")
        ));
        Ok(true)
    }

    fn work_changed(&self) -> Result<bool> {
        Ok(!git(&self.worktree, &["diff", "--name-only", &self.base])?
            .trim()
            .is_empty()
            || !git(
                &self.worktree,
                &["ls-files", "--others", "--exclude-standard"],
            )?
            .trim()
            .is_empty())
    }

    /// The gate only ever narrows a success; a verification failure at
    /// finish outranks whatever the reviewer thought.
    fn map_review(&self, report: &mut OrderReport, decision: Option<ReviewDecision>) {
        if !matches!(report.outcome, Outcome::Verified | Outcome::Completed) {
            return;
        }
        match decision {
            Some(ReviewDecision::Approve(_)) if report.outcome == Outcome::Verified => {
                report.outcome = Outcome::Approved;
            }
            // Completed stays completed: approval cannot substitute for the
            // verification the repository never required.
            Some(ReviewDecision::Approve(_)) | None => {}
            Some(ReviewDecision::Reject(_)) => {
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

    /// Schedule a same-task revision if this failed attempt earns one.
    fn try_revise(&mut self, report: &mut OrderReport) -> Result<bool> {
        let Some(reason) = revision_viable(self.ctx, self.order, report, self.max_attempts) else {
            return Ok(false);
        };
        self.feedback = revision_feedback(report);
        revise(self.ctx, self.order, &self.order_dir, report, reason)?;
        Ok(true)
    }

    /// A rejected order's finish released its claims, so its revision needs
    /// a fresh task; a conflict means another order reclaimed the scope.
    fn rebegin_for_revision(&mut self, report: &mut OrderReport) -> Result<bool> {
        let Some(reason) = revision_viable(self.ctx, self.order, report, self.max_attempts) else {
            return Ok(false);
        };
        self.feedback = revision_feedback(report);
        match self.ctx.grove.task_begin(
            &self.worktree,
            &self.agent,
            &self.order.title,
            &self.order.scope,
            self.order.claim_group.as_deref(),
        )? {
            BeginOutcome::Begun { task } => {
                self.task_id = task.id.clone();
                report.task_id = Some(task.id);
                revise(self.ctx, self.order, &self.order_dir, report, reason)?;
                Ok(true)
            }
            BeginOutcome::Conflict { conflicts } => {
                report.conflicts = Some(serde_json::Value::Array(conflicts));
                report.detail = Some(format!(
                    "{}; revision blocked: scope was reclaimed",
                    report.detail.take().unwrap_or_default()
                ));
                Ok(false)
            }
        }
    }

    fn done(&self, report: &mut OrderReport, abandon: Option<&str>) -> Result<Flow> {
        finalize(
            self.ctx,
            self.order,
            &self.task_id,
            &self.worktree,
            report,
            abandon,
        );
        Ok(Flow::Done)
    }
}

/// Whether this failed attempt earns another try, and why the revision is
/// happening. Denials that matter get recorded on the report.
fn revision_viable(
    ctx: &Ctx,
    order: &Order,
    report: &mut OrderReport,
    max_attempts: u64,
) -> Option<&'static str> {
    let note = |report: &mut OrderReport, text: &str| {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; {text}"),
            None => text.to_string(),
        });
    };
    if report.attempts >= max_attempts {
        return None;
    }
    if SHUTDOWN.load(Ordering::SeqCst) {
        note(report, "shutdown before the revision could dispatch");
        return None;
    }
    // A cap means "spend no more once reached", so equality blocks too.
    if let (Some(cap), Some(used)) = (order.max_tokens, report.usage_tokens)
        && used >= cap
    {
        note(
            report,
            &format!("order token budget reached ({used} of {cap}) — not revised"),
        );
        return None;
    }
    if let Some(budget) = ctx.config.run_token_budget() {
        let spent = ctx.spent.load(Ordering::SeqCst);
        if spent >= budget {
            note(
                report,
                &format!("run token budget exhausted ({spent} of {budget}) — not revised"),
            );
            return None;
        }
    }
    Some(match report.outcome {
        Outcome::Rejected => "rejected",
        _ => "unverified",
    })
}

/// The evidence the next attempt must address: reviewer findings when the
/// gate rejected, the verification failure otherwise.
fn revision_feedback(report: &OrderReport) -> String {
    if let Some(review) = &report.review
        && review.verdict == "reject"
    {
        format!(
            "Reviewer findings:\n{}",
            serde_json::to_string_pretty(&review.findings).unwrap_or_default()
        )
    } else {
        format!(
            "Verification failure: {}",
            report.detail.as_deref().unwrap_or("unspecified")
        )
    }
}

/// Reset the report to a clean slate for the next attempt. Everything an
/// attempt produced is cleared — a second attempt failing before review must
/// not report the first attempt's verdict, and a later reviewer must not see
/// receipts from a superseded attempt. (Callers compute the revision
/// feedback from this state BEFORE calling.)
fn revise(
    ctx: &Ctx,
    order: &Order,
    order_dir: &Path,
    report: &mut OrderReport,
    reason: &str,
) -> Result<()> {
    report.attempts += 1;
    report.detail = None;
    report.finish = None;
    report.review = None;
    report.verify.clear();
    report.tripwires.clear();
    report.executor_exit = None;
    let prefix = format!("r{}-", report.attempts);
    ctx.events.emit(
        "order_revised",
        serde_json::json!({
            "id": order.id,
            "attempt": report.attempts,
            "reason": reason,
            "task_id": report.task_id,
            // The next attempt's logs, so a live consumer can keep tailing.
            "stdout_log": order_dir.join(format!("{prefix}stdout.log")).display().to_string(),
            "stderr_log": order_dir.join(format!("{prefix}stderr.log")).display().to_string(),
        }),
    )
}

const WORKER_RESULT_WINDOW: usize = 10;

fn parse_worker_result(output: &str) -> Option<Result<(), String>> {
    for line in output
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(WORKER_RESULT_WINDOW)
    {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(status) = value.get("summoner_status") else {
            continue;
        };
        let Some(status) = status.as_str() else {
            return Some(Err(
                "executor emitted a malformed summoner completion result".into(),
            ));
        };
        let Some(unmet) = value.get("unmet").and_then(serde_json::Value::as_array) else {
            return Some(Err(
                "executor completion result is missing an unmet list".into()
            ));
        };
        let Some(unmet) = unmet
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>()
        else {
            return Some(Err(
                "executor completion result has a malformed unmet list".into()
            ));
        };
        return Some(match (status, unmet.is_empty()) {
            ("complete", true) => Ok(()),
            ("complete", false) | ("incomplete", false) => Err(format!(
                "executor reported unmet acceptance: {}",
                unmet.join("; ")
            )),
            ("incomplete", true) => Err("executor reported incomplete work".into()),
            _ => Err(format!(
                "executor reported unknown summoner status {status:?}"
            )),
        });
    }
    None
}

#[cfg(test)]
mod worker_result_tests {
    use super::parse_worker_result;

    #[test]
    fn complete_requires_an_empty_unmet_list() {
        assert!(matches!(
            parse_worker_result(r#"{"summoner_status":"complete","unmet":[]}"#),
            Some(Ok(()))
        ));
        assert!(matches!(
            parse_worker_result(
                r#"{"summoner_status":"complete","unmet":["wire test"]}"#
            ),
            Some(Err(reason)) if reason.contains("wire test")
        ));
    }

    #[test]
    fn incomplete_and_malformed_results_fail_closed() {
        assert!(matches!(
            parse_worker_result(
                r#"{"summoner_status":"incomplete","unmet":["Windows artifact"]}"#
            ),
            Some(Err(reason)) if reason.contains("Windows artifact")
        ));
        assert!(matches!(
            parse_worker_result(r#"{"summoner_status":"complete"}"#),
            Some(Err(_))
        ));
    }

    #[test]
    fn only_the_trailing_window_is_trusted() {
        let buried = format!(
            "{}\n{}",
            r#"{"summoner_status":"complete","unmet":[]}"#,
            (0..11).map(|_| "footer").collect::<Vec<_>>().join("\n")
        );
        assert!(parse_worker_result(&buried).is_none());
    }
}
