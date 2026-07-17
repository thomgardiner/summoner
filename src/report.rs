//! The run report: summoner's external JSON contract with the orchestrator.
//! Ranked worst-first so the reviewer reads failures before successes.

use crate::grove::{TaskVerification, VerifySummary};
use serde::Serialize;
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;

/// Variant order is the ranking: worst first. The derived `Ord` drives the
/// report sort, so do not reorder casually.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
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
    /// Verification failed or finish still lacked required evidence.
    Unverified,
    /// Operator interrupt tore the order down.
    Interrupted,
    /// Never started: the queue drained after an interrupt.
    Skipped,
    /// Finished with an explicit override because the repository declares no
    /// required verification profiles. Review like unverified work.
    Completed,
    /// Finished with fresh receipts for every required profile.
    Verified,
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
            Outcome::Interrupted => "interrupted",
            Outcome::Skipped => "skipped",
            Outcome::Completed => "completed",
            Outcome::Verified => "verified",
        }
    }
}

#[derive(Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub run_id: String,
    pub repo: String,
    pub started_at: u64,
    pub duration_secs: u64,
    pub summary: BTreeMap<&'static str, usize>,
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
    ) -> Self {
        orders.sort_by(|a, b| a.outcome.cmp(&b.outcome).then(a.id.cmp(&b.id)));
        let mut summary = BTreeMap::new();
        for order in &orders {
            *summary.entry(order.outcome.key()).or_insert(0) += 1;
        }
        RunReport {
            schema_version: SCHEMA_VERSION,
            run_id,
            repo,
            started_at,
            duration_secs,
            summary,
            orders,
        }
    }

    /// 0 only when every order carries fresh receipts; `completed` still means
    /// "review me", so it exits 1 like every other non-verified outcome.
    pub fn exit_code(&self) -> i32 {
        if self
            .orders
            .iter()
            .all(|order| order.outcome == Outcome::Verified)
        {
            0
        } else {
            1
        }
    }
}

#[derive(Serialize)]
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
    pub commits: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_error: Option<String>,
    pub acceptance: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<VerifySummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<TaskVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<serde_json::Value>,
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
            commits: 0,
            diff: None,
            saved_to: None,
            release_error: None,
            acceptance: order.acceptance.clone(),
            verify: Vec::new(),
            finish: None,
            conflicts: None,
            executor_exit: None,
            stdout_log: None,
            stderr_log: None,
            stdout_tail: None,
            stderr_tail: None,
            timing: Timing::default(),
        }
    }
}

#[derive(Serialize, Default)]
pub struct DiffStats {
    pub files_changed: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub uncommitted_files: u64,
}

#[derive(Serialize, Default)]
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
            timeout_secs: None,
            base: None,
            branch: None,
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
        );
        assert_eq!(green.exit_code(), 0);
    }
}
