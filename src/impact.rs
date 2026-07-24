//! Delivery economics vs a recorded baseline.
//!
//! Summoner does not invent impact numbers. Operators write a baseline snapshot
//! after a known period, then compare later aggregates. Missing baseline means
//! "no comparison yet", not zero improvement.

use crate::run;
use crate::scorecard::{self, ExecutorStats};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
pub struct Baseline {
    pub schema_version: u32,
    pub captured_at: u64,
    pub repo_filter: Option<String>,
    /// repo -> executor -> stats
    pub board: BTreeMap<String, BTreeMap<String, StatsSnapshot>>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StatsSnapshot {
    pub orders: u64,
    pub green: u64,
    pub attempts: u64,
    pub usage_tokens: u64,
}

#[derive(Serialize)]
struct Delta {
    repo: String,
    executor: String,
    orders: i64,
    green: i64,
    green_rate_before: Option<f64>,
    green_rate_after: Option<f64>,
    green_rate_delta: Option<f64>,
    tokens_per_order_before: Option<f64>,
    tokens_per_order_after: Option<f64>,
    tokens_per_order_delta: Option<f64>,
}

pub fn run(
    repo_filter: Option<&str>,
    baseline_path: Option<&Path>,
    write_baseline: Option<&Path>,
) -> Result<i32> {
    let root = run::runs_root();
    let path = root.join("scorecard.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let board = scorecard::aggregate(&text, repo_filter);

    if let Some(path) = write_baseline {
        let baseline = Baseline {
            schema_version: 1,
            captured_at: now_secs(),
            repo_filter: repo_filter.map(str::to_string),
            board: snapshot_board(&board),
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&baseline)?)
            .with_context(|| format!("writing baseline {}", path.display()))?;
        println!(
            "{}",
            serde_json::json!({
                "wrote_baseline": path.display().to_string(),
                "repos": baseline.board.len(),
                "captured_at": baseline.captured_at,
            })
        );
        return Ok(0);
    }

    let Some(baseline_path) = baseline_path else {
        println!(
            "{}",
            serde_json::json!({
                "current": snapshot_board(&board),
                "note": "pass --baseline PATH to compare, or --write-baseline PATH to record one",
            })
        );
        return Ok(0);
    };

    let baseline: Baseline = serde_json::from_slice(
        &std::fs::read(baseline_path)
            .with_context(|| format!("reading baseline {}", baseline_path.display()))?,
    )
    .context("parsing baseline")?;
    if baseline.schema_version != 1 {
        bail!(
            "unsupported impact baseline schema {}",
            baseline.schema_version
        );
    }

    let current = snapshot_board(&board);
    let mut deltas = Vec::new();
    for (repo, executors) in &current {
        for (executor, after) in executors {
            let before = baseline
                .board
                .get(repo)
                .and_then(|m| m.get(executor))
                .cloned()
                .unwrap_or(StatsSnapshot {
                    orders: 0,
                    green: 0,
                    attempts: 0,
                    usage_tokens: 0,
                });
            deltas.push(Delta {
                repo: repo.clone(),
                executor: executor.clone(),
                orders: after.orders as i64 - before.orders as i64,
                green: after.green as i64 - before.green as i64,
                green_rate_before: rate(before.green, before.orders),
                green_rate_after: rate(after.green, after.orders),
                green_rate_delta: match (
                    rate(before.green, before.orders),
                    rate(after.green, after.orders),
                ) {
                    (Some(b), Some(a)) => Some(a - b),
                    _ => None,
                },
                tokens_per_order_before: per_order(before.usage_tokens, before.orders),
                tokens_per_order_after: per_order(after.usage_tokens, after.orders),
                tokens_per_order_delta: match (
                    per_order(before.usage_tokens, before.orders),
                    per_order(after.usage_tokens, after.orders),
                ) {
                    (Some(b), Some(a)) => Some(a - b),
                    _ => None,
                },
            });
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "baseline_path": baseline_path.display().to_string(),
            "baseline_captured_at": baseline.captured_at,
            "deltas": deltas,
            "note": "deltas are descriptive only; they do not prove causation",
        }))?
    );
    Ok(0)
}

fn snapshot_board(
    board: &BTreeMap<String, BTreeMap<String, ExecutorStats>>,
) -> BTreeMap<String, BTreeMap<String, StatsSnapshot>> {
    board
        .iter()
        .map(|(repo, execs)| {
            (
                repo.clone(),
                execs
                    .iter()
                    .map(|(name, s)| {
                        (
                            name.clone(),
                            StatsSnapshot {
                                orders: s.orders,
                                green: s.green,
                                attempts: s.attempts,
                                usage_tokens: s.usage_tokens,
                            },
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

fn rate(green: u64, orders: u64) -> Option<f64> {
    (orders > 0).then(|| green as f64 / orders as f64)
}

fn per_order(tokens: u64, orders: u64) -> Option<f64> {
    (orders > 0).then(|| tokens as f64 / orders as f64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
