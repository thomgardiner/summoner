//! Cross-run executor scorecards: every finished run appends one line per
//! order to a machine-wide JSONL, and `summoner scorecard` aggregates it per
//! repository and executor. This is the memory between runs — "GLM keeps
//! failing scope in this repo" becomes a number the orchestrator can read
//! before picking executors, instead of a hunch.

use crate::report::RunReport;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// Append this run's per-order outcomes. Appends are line-atomic and
/// best-effort: the scorecard must never fail the run it describes.
pub(crate) fn record(root: &Path, report: &RunReport) {
    let path = root.join("scorecard.jsonl");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    let Ok(mut file) = file else {
        eprintln!("summoner: cannot append scorecard at {}", path.display());
        return;
    };
    for order in &report.orders {
        let line = serde_json::json!({
            "ts": report.started_at,
            "run_id": report.run_id,
            "repo": report.repo,
            "id": order.id,
            "executor": order.executor,
            "outcome": order.outcome.key(),
            "attempts": order.attempts,
            "usage_tokens": order.usage_tokens,
        });
        let _ = writeln!(file, "{line}");
    }
}

#[derive(serde::Serialize, Default)]
pub struct ExecutorStats {
    pub orders: u64,
    /// Orders that ended green (`verified` or `approved`).
    pub green: u64,
    pub attempts: u64,
    pub usage_tokens: u64,
    pub outcomes: BTreeMap<String, u64>,
}

/// repo -> executor -> stats, optionally filtered to repos whose path
/// contains `repo_filter`.
pub fn aggregate(
    lines: &str,
    repo_filter: Option<&str>,
) -> BTreeMap<String, BTreeMap<String, ExecutorStats>> {
    let mut board: BTreeMap<String, BTreeMap<String, ExecutorStats>> = BTreeMap::new();
    for line in lines.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let repo = entry["repo"].as_str().unwrap_or_default();
        if repo.is_empty() || repo_filter.is_some_and(|filter| !repo.contains(filter)) {
            continue;
        }
        let executor = entry["executor"].as_str().unwrap_or("?");
        let outcome = entry["outcome"].as_str().unwrap_or("?");
        let stats = board
            .entry(repo.to_string())
            .or_default()
            .entry(executor.to_string())
            .or_default();
        stats.orders += 1;
        if matches!(outcome, "verified" | "approved") {
            stats.green += 1;
        }
        stats.attempts += entry["attempts"].as_u64().unwrap_or(1);
        stats.usage_tokens += entry["usage_tokens"].as_u64().unwrap_or(0);
        *stats.outcomes.entry(outcome.to_string()).or_insert(0) += 1;
    }
    board
}

pub fn scorecard(repo_filter: Option<String>) -> Result<i32> {
    let path = crate::run::runs_root().join("scorecard.jsonl");
    let lines = match std::fs::read_to_string(&path) {
        Ok(lines) => lines,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let board = aggregate(&lines, repo_filter.as_deref());
    println!("{}", serde_json::to_string_pretty(&board)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_aggregate_per_repo_and_executor_with_green_counts() {
        let lines = r#"
{"ts":1,"repo":"/repo/a","id":"x","executor":"codex","outcome":"approved","attempts":2,"usage_tokens":500}
{"ts":2,"repo":"/repo/a","id":"y","executor":"codex","outcome":"rejected","attempts":2,"usage_tokens":900}
{"ts":3,"repo":"/repo/a","id":"z","executor":"glm","outcome":"verified","attempts":1,"usage_tokens":100}
{"ts":4,"repo":"/repo/b","id":"w","executor":"glm","outcome":"scope_violation","attempts":1}
not json
"#;
        let board = aggregate(lines, None);
        let codex = &board["/repo/a"]["codex"];
        assert_eq!(codex.orders, 2);
        assert_eq!(codex.green, 1);
        assert_eq!(codex.attempts, 4);
        assert_eq!(codex.usage_tokens, 1400);
        assert_eq!(codex.outcomes["rejected"], 1);
        assert_eq!(board["/repo/b"]["glm"].outcomes["scope_violation"], 1);

        // The filter narrows by substring, so a repo path fragment works.
        let only_b = aggregate(lines, Some("repo/b"));
        assert!(!only_b.contains_key("/repo/a"));
        assert_eq!(only_b["/repo/b"]["glm"].orders, 1);
    }
}
