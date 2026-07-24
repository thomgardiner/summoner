//! Plan which candidates land and in what order.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::{Candidate, Plan};
use crate::report::is_green_outcome;

pub(crate) fn candidates(report: &Value) -> Result<Vec<Candidate>> {
    let orders = report["orders"]
        .as_array()
        .context("report.json has no orders array")?;
    Ok(orders
        .iter()
        .filter_map(|order| {
            Some(Candidate {
                id: order["id"].as_str()?.to_string(),
                outcome: order["outcome"].as_str().unwrap_or("").to_string(),
                commit: order["candidate_commit"].as_str().map(String::from),
                after: order["after"]
                    .as_array()
                    .map(|deps| {
                        deps.iter()
                            .filter_map(|dep| dep.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect())
}

/// Decide the landable set and its dependency order, and record why each other
/// order was set aside. Pure over the candidate list so the policy is testable
/// without a repository.
///
/// An order lands only if it is green, carries a candidate commit, and its whole
/// `after` closure lands too — you cannot integrate work built on a dependency
/// that itself did not pass. A dependency cycle (which order validation already
/// rejects upstream) is set aside rather than looped on.
pub(crate) fn plan_landing(candidates: Vec<Candidate>) -> Plan {
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut green: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in candidates {
        if !is_green_outcome(&candidate.outcome) {
            skipped.push((candidate.id, format!("outcome {}", candidate.outcome)));
        } else if candidate.commit.is_none() {
            skipped.push((candidate.id, "no candidate commit".to_string()));
        } else {
            green.insert(candidate.id.clone(), candidate);
        }
    }

    // Drop any green order that depends on one not landing, to a fixpoint so the
    // block propagates down the chain.
    loop {
        let doomed: Vec<(String, String)> = green
            .values()
            .filter_map(|candidate| {
                candidate
                    .after
                    .iter()
                    .find(|dep| !green.contains_key(dep.as_str()))
                    .map(|dep| {
                        (
                            candidate.id.clone(),
                            format!("dependency {dep} did not land"),
                        )
                    })
            })
            .collect();
        if doomed.is_empty() {
            break;
        }
        for (id, reason) in doomed {
            green.remove(&id);
            skipped.push((id, reason));
        }
    }

    // Kahn topological order over the survivors; deps land before dependents.
    // Each pass moves the ready candidates out of `green`, so the loop runs
    // until `green` is drained rather than against its shrinking length.
    let mut order = Vec::new();
    let mut landed: BTreeSet<String> = BTreeSet::new();
    while !green.is_empty() {
        let ready: Vec<String> = green
            .values()
            .filter(|candidate| candidate.after.iter().all(|dep| landed.contains(dep)))
            .map(|candidate| candidate.id.clone())
            .collect();
        if ready.is_empty() {
            // A cycle among survivors: set the rest aside instead of looping.
            for candidate in green.values() {
                skipped.push((candidate.id.clone(), "dependency cycle".to_string()));
            }
            break;
        }
        for id in ready {
            landed.insert(id.clone());
            order.push(green.remove(&id).expect("ready id is present"));
        }
    }

    skipped.sort();
    Plan { order, skipped }
}
