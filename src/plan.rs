//! `summoner plan`: the refutation step between decomposing and dispatching.
//! Orders in, grove's partition verdict out, plus the delta between the
//! dependency edges the orders declare and the ones the workspace demands.
//! Revise until clean, then `summoner run` the same files.

use crate::config::Config;
use crate::grove::GroveCli;
use crate::order::{self, Order};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
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

    let sets: Vec<serde_json::Value> = orders
        .iter()
        .map(|order| serde_json::json!({"id": order.id, "scope": order.scope}))
        .collect();
    let repo = std::env::current_dir().context("resolving current directory")?;
    let partition = grove.partition(&repo, &serde_json::Value::Array(sets))?;

    let missing_after = missing_after(&orders, &partition);
    let conflicts = partition["conflicts"]
        .as_array()
        .is_some_and(|c| !c.is_empty());
    let clean = problems.is_empty() && missing_after.is_empty() && !conflicts;
    let report = PlanReport {
        verdict: if clean { "clean" } else { "revise" },
        problems,
        missing_after,
        partition,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(if clean { 0 } else { 1 })
}

/// Which suggested edges the order files do not already declare. Declared
/// extras are left alone: an orchestrator may know ordering reasons the
/// package graph cannot see.
fn missing_after(orders: &[Order], partition: &serde_json::Value) -> Vec<MissingAfter> {
    let declared: std::collections::BTreeMap<&str, BTreeSet<&str>> = orders
        .iter()
        .map(|order| {
            (
                order.id.as_str(),
                order.after.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let Some(suggested) = partition["suggested_after"].as_array() else {
        return Vec::new();
    };
    suggested
        .iter()
        .filter_map(|edge| {
            let id = edge["id"].as_str()?;
            let empty = BTreeSet::new();
            let have = declared.get(id).unwrap_or(&empty);
            let missing: Vec<String> = edge["after"]
                .as_array()?
                .iter()
                .filter_map(|dep| dep.as_str())
                .filter(|dep| !have.contains(dep))
                .map(String::from)
                .collect();
            (!missing.is_empty()).then(|| MissingAfter {
                id: id.to_string(),
                missing,
            })
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
    fn declared_edges_satisfy_suggestions_and_missing_ones_surface() {
        let partition = serde_json::json!({
            "suggested_after": [
                {"id": "app", "after": ["core", "util"]},
                {"id": "docs", "after": ["app"]},
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
}
