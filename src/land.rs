//! `summoner land`: integrate a finished run's verified candidate branches into
//! the current branch, in dependency order, stopping at the first real conflict.
//!
//! This is the gated apply, not an auto-merge: it only touches candidates that
//! already passed the run's bar (verified, or approved when a reviewer ran), and
//! it merges the exact `candidate_commit` the report recorded — the commit that
//! was reviewed, not whatever the branch points at now. A non-green order and
//! everything downstream of it are set aside with a reason. The first conflict
//! stops the run with the earlier merges already committed, so progress is never
//! lost and the working tree is always left clean.

use crate::report::is_green_outcome;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// One order's landing-relevant facts, projected from `report.json`.
struct Candidate {
    id: String,
    outcome: String,
    commit: Option<String>,
    after: Vec<String>,
}

/// The decided plan: what to merge and in what order, and what was set aside.
struct Plan {
    /// Landable candidates in dependency (topological) order.
    order: Vec<Candidate>,
    /// `(id, reason)` for every order not landed.
    skipped: Vec<(String, String)>,
}

pub fn land(run_id: Option<String>, dry_run: bool) -> Result<i32> {
    let root = crate::run::runs_root();
    let run_dir = match run_id {
        Some(id) => root.join(id),
        None => latest_finished_run(&root)?,
    };
    let report_path = run_dir.join("report.json");
    let report: Value = serde_json::from_slice(
        &std::fs::read(&report_path)
            .with_context(|| format!("reading {}", report_path.display()))?,
    )
    .with_context(|| format!("parsing {}", report_path.display()))?;

    let repo = report["repo"]
        .as_str()
        .context("report.json has no repo")?
        .to_string();
    // Land into the repository the run targeted, and only from there: the
    // candidate commits live in that repository's object database.
    let here = git(Path::new("."), &["rev-parse", "--show-toplevel"])
        .context("summoner land must run inside a git repository")?;
    if canonical(&here) != canonical(&repo) {
        bail!("this run targeted {repo}, but you are in {here}; run `summoner land` there");
    }
    let repo = PathBuf::from(&repo);

    let candidates = candidates(&report)?;
    let plan = plan_landing(candidates);

    if plan.order.is_empty() {
        report_result(&repo, &plan, &[], None, dry_run)?;
        // Nothing to land is a clean no-op, not a failure.
        return Ok(0);
    }

    if dry_run {
        report_result(&repo, &plan, &[], None, true)?;
        return Ok(0);
    }

    // Refuse to merge into work in progress: a dirty tree makes an aborted merge
    // impossible to reason about.
    if !git(&repo, &["status", "--porcelain"])?.is_empty() {
        bail!("working tree is not clean; commit or stash before landing");
    }

    let mut landed: Vec<Value> = Vec::new();
    let mut stopped: Option<Value> = None;
    for candidate in &plan.order {
        let commit = candidate.commit.as_deref().expect("landable has a commit");
        // The reviewed commit must still exist; a reaped worktree that salvaged
        // nothing could have left the report naming a commit that is now gone.
        if git(&repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
            stopped = Some(json!({
                "id": candidate.id,
                "reason": format!("candidate commit {commit} is missing from the repository"),
            }));
            break;
        }
        match merge(&repo, &candidate.id, commit) {
            Ok(mode) => landed.push(json!({"id": candidate.id, "commit": commit, "mode": mode})),
            Err(conflict) => {
                // Leave the tree exactly as it was before this merge.
                let _ = git(&repo, &["merge", "--abort"]);
                stopped = Some(json!({"id": candidate.id, "commit": commit, "reason": conflict}));
                break;
            }
        }
    }

    let stopped_here = stopped.is_some();
    report_result(&repo, &plan, &landed, stopped, false)?;
    // A conflict is a domain refusal the caller must resolve, not a crash.
    Ok(if stopped_here { 1 } else { 0 })
}

/// Project the report's orders into landing candidates.
fn candidates(report: &Value) -> Result<Vec<Candidate>> {
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
fn plan_landing(candidates: Vec<Candidate>) -> Plan {
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

/// Merge one candidate commit, letting git fast-forward when it can. Returns the
/// mode ("fast-forward" or "merge") on success, or the conflict message on
/// failure without aborting — the caller decides how to clean up.
fn merge(repo: &Path, id: &str, commit: &str) -> Result<&'static str, String> {
    let output = Command::new("git")
        .args([
            "merge",
            "--no-edit",
            "-m",
            &format!("summoner: land order {id} ({commit})"),
            commit,
        ])
        .current_dir(repo)
        .output()
        .map_err(|error| format!("running git merge: {error}"))?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(if text.contains("Fast-forward") {
            "fast-forward"
        } else {
            "merge"
        })
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("merge failed")
            .trim()
            .to_string())
    }
}

fn report_result(
    repo: &Path,
    plan: &Plan,
    landed: &[Value],
    stopped: Option<Value>,
    dry_run: bool,
) -> Result<()> {
    let head = git(repo, &["rev-parse", "HEAD"]).unwrap_or_default();
    let planned: Vec<&str> = plan.order.iter().map(|c| c.id.as_str()).collect();
    let skipped: Vec<Value> = plan
        .skipped
        .iter()
        .map(|(id, reason)| json!({"id": id, "reason": reason}))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "dry_run": dry_run,
            "repo": repo.display().to_string(),
            "head": head,
            "planned": planned,
            "landed": landed,
            "skipped": skipped,
            "stopped": stopped,
        }))?
    );
    Ok(())
}

fn latest_finished_run(root: &Path) -> Result<PathBuf> {
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root)
        .with_context(|| format!("no runs under {}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join("report.json").exists())
        .collect();
    runs.sort();
    runs.pop().with_context(|| {
        format!(
            "no finished run with a report.json under {}",
            root.display()
        )
    })
}

/// git that bails on failure, for the read-only queries and clean-tree check.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, outcome: &str, commit: Option<&str>, after: &[&str]) -> Candidate {
        Candidate {
            id: id.to_string(),
            outcome: outcome.to_string(),
            commit: commit.map(String::from),
            after: after.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn ids(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.id.as_str()).collect()
    }

    #[test]
    fn dependencies_land_before_dependents() {
        let plan = plan_landing(vec![
            candidate("c", "verified", Some("c1"), &["b"]),
            candidate("a", "approved", Some("a1"), &[]),
            candidate("b", "verified", Some("b1"), &["a"]),
        ]);
        assert_eq!(ids(&plan.order), ["a", "b", "c"]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_non_green_order_and_its_dependents_are_skipped() {
        let plan = plan_landing(vec![
            candidate("base", "verified", Some("base1"), &[]),
            candidate("broken", "rejected", None, &["base"]),
            candidate("downstream", "verified", Some("d1"), &["broken"]),
        ]);
        assert_eq!(ids(&plan.order), ["base"]);
        assert_eq!(
            plan.skipped,
            vec![
                ("broken".to_string(), "outcome rejected".to_string()),
                (
                    "downstream".to_string(),
                    "dependency broken did not land".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_green_order_without_a_candidate_commit_is_skipped() {
        let plan = plan_landing(vec![candidate("a", "verified", None, &[])]);
        assert!(plan.order.is_empty());
        assert_eq!(
            plan.skipped,
            vec![("a".to_string(), "no candidate commit".to_string())]
        );
    }

    #[test]
    fn independent_green_orders_all_land() {
        let plan = plan_landing(vec![
            candidate("y", "verified", Some("y1"), &[]),
            candidate("x", "verified", Some("x1"), &[]),
        ]);
        assert_eq!(ids(&plan.order), ["x", "y"]);
    }

    #[test]
    fn a_cycle_is_set_aside_not_looped() {
        let plan = plan_landing(vec![
            candidate("a", "verified", Some("a1"), &["b"]),
            candidate("b", "verified", Some("b1"), &["a"]),
        ]);
        assert!(plan.order.is_empty());
        assert_eq!(
            plan.skipped,
            vec![
                ("a".to_string(), "dependency cycle".to_string()),
                ("b".to_string(), "dependency cycle".to_string()),
            ]
        );
    }
}
