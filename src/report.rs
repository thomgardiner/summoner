use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Clone)]
pub struct CommandEvidence {
    pub argv: Vec<String>,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub interrupted: bool,
    pub success: bool,
    pub duration_ms: u128,
    pub stdout_log: String,
    pub stderr_log: String,
}

#[derive(Debug, Serialize)]
pub struct OrderReport {
    pub id: String,
    pub title: String,
    pub outcome: String,
    pub detail: Option<String>,
    pub after: Vec<String>,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    pub candidate_commit: Option<String>,
    pub changed_paths: Vec<String>,
    pub worktree: Option<String>,
    pub executor: Option<CommandEvidence>,
    pub verify: Vec<CommandEvidence>,
}

impl OrderReport {
    pub fn skipped(order: &crate::order::Order, detail: impl Into<String>) -> Self {
        Self {
            id: order.id.clone(),
            title: order.title.clone(),
            outcome: "skipped".into(),
            detail: Some(detail.into()),
            after: order.after.clone(),
            branch: None,
            base_commit: None,
            candidate_commit: None,
            changed_paths: Vec::new(),
            worktree: None,
            executor: None,
            verify: Vec::new(),
        }
    }
}

pub fn error_report(order: &crate::order::Order, error: anyhow::Error) -> OrderReport {
    OrderReport {
        id: order.id.clone(),
        title: order.title.clone(),
        outcome: "error".into(),
        detail: Some(error.to_string()),
        after: order.after.clone(),
        branch: None,
        base_commit: None,
        candidate_commit: None,
        changed_paths: Vec::new(),
        worktree: None,
        executor: None,
        verify: Vec::new(),
    }
}

pub fn print_event(order: &OrderReport) {
    let event = serde_json::json!({"event": "order_finished", "order": order});
    println!("{event}");
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub repo: String,
    pub jobs: usize,
    pub orders: Vec<OrderReport>,
    pub summary: BTreeMap<String, usize>,
}

impl RunReport {
    pub fn new(run_id: String, repo: String, jobs: usize, mut orders: Vec<OrderReport>) -> Self {
        orders.sort_by(|a, b| a.id.cmp(&b.id));
        let mut summary = BTreeMap::new();
        for order in &orders {
            *summary.entry(order.outcome.clone()).or_insert(0) += 1;
        }
        Self {
            run_id,
            repo,
            jobs,
            orders,
            summary,
        }
    }

    pub fn exit_code(&self) -> i32 {
        if self.orders.iter().all(|order| order.outcome == "verified") {
            0
        } else {
            1
        }
    }
}
