//! `summoner plan`: the refutation step between decomposing and dispatching.
//! Orders in, grove's partition verdict out, plus the delta between the
//! dependency edges the orders declare and the ones the workspace demands.
//! Revise until clean, then `summoner run` the same files.

use crate::config::Config;
use crate::grove::GroveCli;
use crate::order::{self, Order};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Serialize)]
pub struct PlanReport {
    /// "clean" when nothing blocks the batch as written; "revise" otherwise.
    pub verdict: &'static str,
    /// Order-file problems `summoner run` would refuse outright.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// Suggested `after` edges the orders do not declare yet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_after: Vec<MissingAfter>,
    /// grove's partition verdict, verbatim: sets, conflicts, couplings, waves.
    pub partition: serde_json::Value,
}

#[derive(Serialize)]
pub struct MissingAfter {
    pub id: String,
    pub missing: Vec<String>,
}

pub fn plan(config: &Config, paths: &[PathBuf]) -> Result<i32> {
    let grove = GroveCli::new(config.grove_bin());
    grove.preflight()?;
    let orders = order::load(paths)?;
    let problems = order::validate(&orders, config);

    // Variant siblings carry their claim group so grove's partition treats
    // their deliberate overlap as a race, not a conflict — exactly as the
    // claim registry will at dispatch.
    let sets: Vec<serde_json::Value> = orders
        .iter()
        .map(|order| {
            serde_json::json!({
                "id": order.id,
                "scope": order.scope,
                "group": order.claim_group,
            })
        })
        .collect();
    let repo = std::env::current_dir().context("resolving current directory")?;
    let partition = grove.partition(&repo, &serde_json::Value::Array(sets))?;

    let missing_after = missing_after(&orders, &partition);
    let clean = problems.is_empty() && missing_after.is_empty();
    let report = PlanReport {
        verdict: if clean { "clean" } else { "revise" },
        problems,
        missing_after,
        partition,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if clean { 0 } else { 1 })
}

/// Scope conflicts that are not already serialized by the declared DAG.
/// Package couplings remain advisory: isolated worktrees and build lanes make
/// file-disjoint orders safe to execute in parallel.
fn missing_after(orders: &[Order], partition: &serde_json::Value) -> Vec<MissingAfter> {
    let Some(conflicts) = partition["conflicts"].as_array() else {
        return Vec::new();
    };
    let mut missing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for conflict in conflicts {
        let (Some(a), Some(b)) = (conflict["a"].as_str(), conflict["b"].as_str()) else {
            continue;
        };
        if crate::order::depends_on(orders, a, b) || crate::order::depends_on(orders, b, a) {
            continue;
        }
        missing
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
    }
    missing
        .into_iter()
        .map(|(id, dependencies)| MissingAfter {
            id,
            missing: dependencies.into_iter().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn order(id: &str, after: &[&str]) -> Order {
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
            after: after.iter().map(|s| s.to_string()).collect(),
            source: PathBuf::from(format!("{id}.toml")),
        }
    }

    #[test]
    fn declared_edges_satisfy_conflicts_and_missing_ones_surface() {
        let partition = serde_json::json!({
            "conflicts": [
                {"a": "core", "b": "app", "overlap": ["src/a.rs"]},
                {"a": "util", "b": "app", "overlap": ["src/b.rs"]},
                {"a": "app", "b": "docs", "overlap": ["src/c.rs"]}
            ]
        });
        let orders = [
            order("core", &[]),
            order("util", &[]),
            order("app", &["core"]),
            order("docs", &["app"]),
        ];
        let missing = missing_after(&orders, &partition);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "app");
        assert_eq!(missing[0].missing, ["util"]);
    }

    #[test]
    fn package_couplings_are_advisory() {
        let partition = serde_json::json!({
            "couplings": [
                {
                    "upstream": "core",
                    "downstream": "app",
                    "kind": "dependency",
                    "via": ["core"]
                },
                {
                    "upstream": "left",
                    "downstream": "right",
                    "kind": "same_package",
                    "via": ["one-crate"]
                }
            ],
            "suggested_after": [
                {"id": "app", "after": ["core"]},
                {"id": "right", "after": ["left"]}
            ],
            "conflicts": []
        });
        let orders = [
            order("core", &[]),
            order("app", &[]),
            order("left", &[]),
            order("right", &[]),
        ];

        assert!(missing_after(&orders, &partition).is_empty());
    }

    #[test]
    fn ordered_scope_conflict_needs_no_additional_edge() {
        let partition = serde_json::json!({
            "conflicts": [{"a": "base", "b": "followup", "overlap": ["src/lib.rs"]}]
        });
        let orders = [order("base", &[]), order("followup", &["base"])];

        assert!(missing_after(&orders, &partition).is_empty());
    }

    #[test]
    fn unordered_scope_conflict_requires_an_edge() {
        let partition = serde_json::json!({
            "conflicts": [{"a": "base", "b": "followup", "overlap": ["src/lib.rs"]}]
        });
        let orders = [order("base", &[]), order("followup", &[])];

        let missing = missing_after(&orders, &partition);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "followup");
        assert_eq!(missing[0].missing, ["base"]);
    }
}
