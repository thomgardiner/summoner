//! The run report: summoner's external JSON contract with the orchestrator.
//! Ranked worst-first so the reviewer reads failures before successes.

use crate::grove::{TaskVerification, VerifySummary};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

/// Variant order is the ranking: worst first. The derived `Ord` drives the
/// report sort, so do not reorder casually.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Summoner-side failure (acquire failed, grove unreachable mid-run).
    Error,
    /// Scope conflict at task begin; nothing ran.
    Blocked,
    /// Killed at the deadline (grove exit 124).
    Stalled,
    /// Executor exited nonzero; verification skipped.
    ExecutorFailed,
    /// Finish refused: writes outside the declared scope.
    ScopeViolation,
    /// Verification failed, finish lacked required evidence, or a protected
    /// verification-config file was modified (receipts untrustworthy).
    Unverified,
    /// A reviewer was configured but the gate could not produce a valid
    /// verdict (timeout, no verdict line, or the reviewer wrote to the tree).
    ReviewFailed,
    /// The independent reviewer rejected verified work; findings say why.
    Rejected,
    /// Operator interrupt tore the order down.
    Interrupted,
    /// Never started: the queue drained after an interrupt.
    Skipped,
    /// Finished with an explicit override because the repository declares no
    /// required verification profiles. Review like unverified work.
    Completed,
    /// Finished with fresh receipts for every required profile.
    Verified,
    /// Verified AND approved by the independent reviewer.
    Approved,
}

impl Outcome {
    pub fn key(self) -> &'static str {
        match self {
            Outcome::Error => "error",
            Outcome::Blocked => "blocked",
            Outcome::Stalled => "stalled",
            Outcome::ExecutorFailed => "executor_failed",
            Outcome::ScopeViolation => "scope_violation",
            Outcome::Unverified => "unverified",
            Outcome::ReviewFailed => "review_failed",
            Outcome::Rejected => "rejected",
            Outcome::Interrupted => "interrupted",
            Outcome::Skipped => "skipped",
            Outcome::Completed => "completed",
            Outcome::Verified => "verified",
            Outcome::Approved => "approved",
        }
    }

    /// A fully successful order: verified, and approved when a reviewer ran.
    /// Everything else is non-green (a failure, refusal, or interruption).
    pub fn is_green(self) -> bool {
        matches!(self, Outcome::Verified | Outcome::Approved)
    }
}

/// The [`Outcome::is_green`] test by its serialized `key`, for consumers that
/// hold the journal's outcome string rather than the enum (the notifier).
pub fn is_green_outcome(key: &str) -> bool {
    matches!(key, "verified" | "approved")
}

#[derive(Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub run_id: String,
    pub repo: String,
    pub started_at: u64,
    pub duration_secs: u64,
    pub summary: BTreeMap<&'static str, usize>,
    /// Sum of per-order token usage, present when any executor reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_tokens: Option<u64>,
    /// Content address of the trusted policy that gated this run, when one was
    /// declared: the report states which bar its outcomes were judged against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_policy_sha256: Option<String>,
    pub orders: Vec<OrderReport>,
}

impl RunReport {
    /// Sorts orders worst-first (ties by id) and derives the summary counts.
    pub fn assemble(
        run_id: String,
        repo: String,
        started_at: u64,
        duration_secs: u64,
        mut orders: Vec<OrderReport>,
        trusted_policy_sha256: Option<String>,
    ) -> Self {
        orders.sort_by(|a, b| a.outcome.cmp(&b.outcome).then(a.id.cmp(&b.id)));
        let mut summary = BTreeMap::new();
        for order in &orders {
            *summary.entry(order.outcome.key()).or_insert(0) += 1;
        }
        let usage_tokens = orders
            .iter()
            .filter_map(|order| order.usage_tokens)
            .reduce(|a, b| a.saturating_add(b));
        RunReport {
            schema_version: SCHEMA_VERSION,
            run_id,
            repo,
            started_at,
            duration_secs,
            summary,
            usage_tokens,
            trusted_policy_sha256,
            orders,
        }
    }

    /// 0 only when every order carries fresh receipts (with reviewer approval
    /// where a reviewer gated it); `completed` still means "review me", so it
    /// exits 1 like every other non-verified outcome.
    pub fn exit_code(&self) -> i32 {
        if self.orders.iter().all(|order| order.outcome.is_green()) {
            0
        } else {
            1
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct OrderReport {
    pub id: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub order_file: String,
    pub executor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The candidate's exact commit, captured in the worktree before release.
    /// Release may salvage dirty state into a new commit and advance the
    /// branch, so the branch name alone does not identify what was reviewed;
    /// this does, and it is what any later accept step must integrate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_commit: Option<String>,
    /// How many executor attempts this order took (1 = no revisions).
    pub attempts: u64,
    /// The executor's own session identifier, when a `session_marker`
    /// captured one — revisions resume it, and so can the orchestrator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub commits: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_error: Option<String>,
    pub acceptance: Vec<String>,
    /// The original order id this entry was expanded from (N-version dispatch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_of: Option<String>,
    /// Deterministic diff-scan findings (deleted tests, skip markers, config
    /// edits) surfaced to the reviewer and the orchestrator alike.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tripwires: Vec<String>,
    /// The independent review, when a reviewer gated this order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<VerifySummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<TaskVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor_exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// Structured evidence when the order failed inside a scheduler worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_failure: Option<WorkerFailure>,
    pub timing: Timing,
}

impl OrderReport {
    pub fn new(order: &crate::order::Order, executor: String) -> Self {
        OrderReport {
            id: order.id.clone(),
            outcome: Outcome::Error,
            detail: None,
            order_file: order.source.display().to_string(),
            executor,
            task_id: None,
            worktree: None,
            branch: None,
            base_commit: None,
            candidate_commit: None,
            attempts: 1,
            session_id: None,
            commits: 0,
            diff: None,
            saved_to: None,
            release_error: None,
            acceptance: order.acceptance.clone(),
            variant_of: order.variant_of.clone(),
            tripwires: Vec::new(),
            review: None,
            after: order.after.clone(),
            verify: Vec::new(),
            finish: None,
            conflicts: None,
            usage_tokens: None,
            executor_exit: None,
            stdout_log: None,
            stderr_log: None,
            stdout_tail: None,
            stderr_tail: None,
            worker_failure: None,
            timing: Timing::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureKind {
    Panic,
    SchedulerPoisoned,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WorkerFailure {
    pub kind: WorkerFailureKind,
    pub message: String,
}

impl WorkerFailure {
    pub(crate) fn panic(payload: Box<dyn Any + Send>) -> Self {
        let message = match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(payload) => match payload.downcast::<&'static str>() {
                Ok(message) => (*message).to_string(),
                Err(_) => "non-string panic payload".to_string(),
            },
        };
        Self {
            kind: WorkerFailureKind::Panic,
            message,
        }
    }

    pub(crate) fn poisoned() -> Self {
        Self {
            kind: WorkerFailureKind::SchedulerPoisoned,
            message: "scheduler lock was poisoned by a worker panic".to_string(),
        }
    }
}

/// One independent review: which backend judged, what it said, and where its
/// full transcript lives.
#[derive(Serialize, Deserialize)]
pub struct ReviewSummary {
    pub reviewer: String,
    /// "approve", "reject", or "failed" (no valid verdict was produced).
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The reviewer's findings, verbatim (severity/file/line/summary objects).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    pub duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_log: Option<String>,
    pub protocol_version: u32,
    pub review_nonce: String,
    pub candidate_snapshot_sha256: String,
    pub diff_sha256: String,
    pub raw_stdout_sha256: String,
    pub capsule_id: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct DiffStats {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub uncommitted_files: u64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Timing {
    pub exec_secs: u64,
    pub verify_secs: u64,
    pub total_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(id: &str, outcome: Outcome) -> OrderReport {
        let order = crate::order::Order {
            id: id.to_string(),
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
            source: std::path::PathBuf::from(format!("{id}.toml")),
        };
        let mut report = OrderReport::new(&order, "fake".into());
        report.outcome = outcome;
        report
    }

    #[test]
    fn orders_rank_worst_first_and_summary_counts() {
        let report = RunReport::assemble(
            "r".into(),
            "/repo".into(),
            0,
            1,
            vec![
                report_with("done", Outcome::Verified),
                report_with("late", Outcome::Stalled),
                report_with("clash", Outcome::Blocked),
                report_with("meh", Outcome::Completed),
            ],
            None,
        );
        let ids: Vec<&str> = report.orders.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["clash", "late", "meh", "done"]);
        assert_eq!(report.summary["verified"], 1);
        assert_eq!(report.summary["stalled"], 1);
        assert_eq!(report.exit_code(), 1);

        let green = RunReport::assemble(
            "r".into(),
            "/repo".into(),
            0,
            1,
            vec![report_with("a", Outcome::Verified)],
            None,
        );
        assert_eq!(green.exit_code(), 0);
    }
}
