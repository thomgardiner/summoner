//! `summoner land`: integrate a finished run's verified candidate commits into
//! the current branch, in dependency order.
//!
//! Landing is not an auto-merge of branch tips. It merges the exact
//! `candidate_commit` each report recorded (the reviewed commit) onto a
//! temporary integration branch, captures an immutable integration candidate
//! `I` (commit + tree + component list), runs the aggregate verify gate against
//! that tree, re-checks that `I` has not moved, and only then fast-forwards the
//! protected target **to that exact commit**. A non-green order and everything
//! downstream of it are set aside. The first conflict aborts without advancing
//! the target.

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
    let (repo, plan) = load_landing_context(&run_dir)?;

    if plan.order.is_empty() {
        report_result(&repo, &plan, &[], None, None, None, dry_run)?;
        return Ok(0);
    }
    if dry_run {
        report_result(&repo, &plan, &[], None, None, None, true)?;
        return Ok(0);
    }
    if !git(&repo, &["status", "--porcelain"])?.is_empty() {
        bail!("working tree is not clean; commit or stash before landing");
    }

    let target_branch = git(&repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("land requires a checked-out branch (not detached HEAD)")?;
    let target_head = git(&repo, &["rev-parse", "HEAD"])?;
    let run_slug = run_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("run");
    let integration = format!(
        "smn/land-integration-{}-{}",
        run_slug,
        target_head.chars().take(8).collect::<String>()
    );
    git(&repo, &["checkout", "-B", &integration, &target_head])?;

    let (landed, mut stopped) = merge_candidates(&repo, &plan.order);
    let (aggregate, integration_candidate, seal_stop) =
        seal_and_gate(&repo, run_slug, &target_head, &landed, stopped.is_none());
    if stopped.is_none() {
        stopped = seal_stop;
    }

    if stopped.is_some() {
        // Drop the temp integration branch; leave only retained refs for sealed I.
        abandon_integration(&repo, &target_branch, &integration);
        report_result(
            &repo,
            &plan,
            &landed,
            stopped,
            aggregate,
            integration_candidate,
            false,
        )?;
        return Ok(1);
    }

    advance_to_integration(
        &repo,
        &target_branch,
        &integration,
        integration_candidate
            .as_ref()
            .context("integration candidate missing after a successful gate")?,
    )?;
    report_result(
        &repo,
        &plan,
        &landed,
        None,
        aggregate,
        integration_candidate,
        false,
    )?;
    Ok(0)
}

fn load_landing_context(run_dir: &Path) -> Result<(PathBuf, Plan)> {
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
    let here = git(Path::new("."), &["rev-parse", "--show-toplevel"])
        .context("summoner land must run inside a git repository")?;
    if canonical(&here) != canonical(&repo) {
        bail!("this run targeted {repo}, but you are in {here}; run `summoner land` there");
    }
    let repo = PathBuf::from(&repo);
    let plan = plan_landing(candidates(&report)?);
    Ok((repo, plan))
}

fn merge_candidates(repo: &Path, order: &[Candidate]) -> (Vec<Value>, Option<Value>) {
    let mut landed = Vec::new();
    for candidate in order {
        let commit = candidate.commit.as_deref().expect("landable has a commit");
        if git(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
            return (
                landed,
                Some(json!({
                    "id": candidate.id,
                    "reason": format!("candidate commit {commit} is missing from the repository"),
                })),
            );
        }
        match merge(repo, &candidate.id, commit) {
            Ok(mode) => landed.push(json!({"id": candidate.id, "commit": commit, "mode": mode})),
            Err(conflict) => {
                let _ = git(repo, &["merge", "--abort"]);
                return (
                    landed,
                    Some(json!({"id": candidate.id, "commit": commit, "reason": conflict})),
                );
            }
        }
    }
    (landed, None)
}

fn seal_and_gate(
    repo: &Path,
    run_slug: &str,
    base_commit: &str,
    landed: &[Value],
    proceed: bool,
) -> (Option<Value>, Option<Value>, Option<Value>) {
    if !proceed {
        return (None, None, None);
    }
    // Capture I without retaining yet — only a gate-passing candidate is retained.
    let mut captured = match capture_integration(repo, run_slug, base_commit, landed) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                None,
                Some(json!({
                    "id": "_integration",
                    "reason": format!("failed to capture integration candidate: {error:#}"),
                })),
            );
        }
    };
    let sealed = captured["integration_commit"]
        .as_str()
        .expect("capture always sets integration_commit")
        .to_string();
    match aggregate_verify(repo) {
        Ok(report) => {
            if let Err(error) = assert_still_at(repo, &sealed) {
                return (
                    None,
                    None,
                    Some(json!({
                        "id": "_integration",
                        "reason": format!("{error:#}"),
                    })),
                );
            }
            if let Err(error) = retain_integration(repo, &mut captured) {
                return (
                    None,
                    None,
                    Some(json!({
                        "id": "_integration",
                        "reason": format!("{error:#}"),
                    })),
                );
            }
            (Some(report), Some(captured), None)
        }
        Err(error) => (
            None,
            None,
            Some(json!({
                "id": "_aggregate",
                "reason": format!("aggregate verify failed: {error:#}"),
            })),
        ),
    }
}

fn advance_to_integration(
    repo: &Path,
    target_branch: &str,
    integration_branch: &str,
    integration_candidate: &Value,
) -> Result<()> {
    let integrated = integration_candidate["integration_commit"]
        .as_str()
        .context("integration candidate missing commit")?
        .to_string();
    assert_still_at(repo, &integrated)?;
    git(repo, &["checkout", target_branch])?;
    git(repo, &["merge", "--ff-only", &integrated])
        .context("fast-forwarding the target branch onto the sealed integration candidate")?;
    let tip = git(repo, &["rev-parse", "HEAD"])?;
    if tip != integrated {
        bail!("target HEAD {tip} is not the sealed integration candidate {integrated}");
    }
    let _ = Command::new("git")
        .args(["branch", "-D", integration_branch])
        .current_dir(repo)
        .output();
    Ok(())
}

/// Capture the post-merge integration candidate without retaining it yet
/// (ASSURANCE I7). Retention happens only after the aggregate gate passes.
fn capture_integration(
    repo: &Path,
    run_slug: &str,
    base_commit: &str,
    landed: &[Value],
) -> Result<Value> {
    if !git(repo, &["status", "--porcelain"])?.is_empty() {
        bail!("integration tree is dirty after merges; refusing to seal I");
    }
    let integration_commit = git(repo, &["rev-parse", "HEAD"])?;
    let integration_tree = git(repo, &["rev-parse", "HEAD^{tree}"])?;
    let components: Vec<Value> = landed
        .iter()
        .map(|entry| {
            json!({
                "id": entry["id"],
                "commit": entry["commit"],
            })
        })
        .collect();
    // Content-addressed id over base + I + ordered components (stable across recapture).
    let identity = {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;
        let mut hash = Sha256::new();
        hash.update(b"summoner.integration-candidate.v1\0");
        hash.update(base_commit.as_bytes());
        hash.update([0]);
        hash.update(integration_commit.as_bytes());
        hash.update([0]);
        hash.update(integration_tree.as_bytes());
        hash.update([0]);
        for entry in landed {
            if let (Some(id), Some(commit)) = (entry["id"].as_str(), entry["commit"].as_str()) {
                hash.update(id.as_bytes());
                hash.update([0]);
                hash.update(commit.as_bytes());
                hash.update([0]);
            }
        }
        let mut hex = String::with_capacity(64);
        for byte in hash.finalize() {
            write!(&mut hex, "{byte:02x}").expect("writing to String");
        }
        hex
    };
    let retained_ref = format!("refs/summoner/integration/{run_slug}");
    Ok(json!({
        "schema_version": 1,
        "integration_id": identity,
        "run_id": run_slug,
        "base_commit": base_commit,
        "integration_commit": integration_commit,
        "integration_tree": integration_tree,
        "components": components,
        "retained_ref": retained_ref,
    }))
}

/// Retain I under `refs/summoner/integration/<run>` only after the gate passes.
/// Refuse to overwrite a different previously sealed I for the same run.
fn retain_integration(repo: &Path, captured: &mut Value) -> Result<()> {
    let retained_ref = captured["retained_ref"]
        .as_str()
        .context("integration candidate missing retained_ref")?
        .to_string();
    let commit = captured["integration_commit"]
        .as_str()
        .context("integration candidate missing commit")?
        .to_string();
    match git(repo, &["rev-parse", "--verify", &retained_ref]) {
        Ok(existing) if existing == commit => Ok(()),
        Ok(existing) => bail!(
            "integration ref {retained_ref} already seals {existing}; refusing to overwrite with {commit}"
        ),
        Err(_) => {
            git(repo, &["update-ref", &retained_ref, &commit])
                .with_context(|| format!("retaining integration candidate under {retained_ref}"))?;
            Ok(())
        }
    }
}

fn abandon_integration(repo: &Path, target_branch: &str, integration_branch: &str) {
    let _ = git(repo, &["checkout", target_branch]);
    let _ = Command::new("git")
        .args(["branch", "-D", integration_branch])
        .current_dir(repo)
        .output();
}

fn assert_still_at(repo: &Path, expected: &str) -> Result<()> {
    let head = git(repo, &["rev-parse", "HEAD"])?;
    if head != expected {
        bail!("integration candidate drifted during gating: expected {expected}, HEAD is {head}");
    }
    Ok(())
}

/// Post-integration gate before the protected target is advanced.
///
/// - `SUMMONER_LAND_VERIFY` — shell-free argv (space-separated, or `\x1f`-joined)
/// - else `cargo test --locked` when a root `Cargo.toml` exists
/// - else refuse (no silent no-op). Escape hatch for fixtures:
///   `SUMMONER_LAND_ALLOW_NO_AGGREGATE=1`
fn aggregate_verify(repo: &Path) -> Result<Value> {
    if let Ok(raw) = std::env::var("SUMMONER_LAND_VERIFY") {
        let argv: Vec<&str> = if raw.contains('\u{1f}') {
            raw.split('\u{1f}').filter(|s| !s.is_empty()).collect()
        } else {
            raw.split_whitespace().collect()
        };
        if argv.is_empty() {
            bail!("SUMMONER_LAND_VERIFY is empty");
        }
        let output = Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(repo)
            .output()
            .with_context(|| format!("running land verify {}", argv[0]))?;
        if !output.status.success() {
            bail!(
                "{} exited {:?}: {}",
                argv[0],
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(json!({
            "command": argv,
            "passed": true,
        }));
    }
    if repo.join("Cargo.toml").is_file() {
        let output = Command::new("cargo")
            .args(["test", "--locked", "--", "--test-threads=1"])
            .current_dir(repo)
            .output()
            .context("running cargo test as land aggregate verify")?;
        if !output.status.success() {
            bail!(
                "cargo test exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(json!({
            "command": ["cargo", "test", "--locked"],
            "passed": true,
        }));
    }
    if std::env::var_os("SUMMONER_LAND_ALLOW_NO_AGGREGATE").is_some() {
        return Ok(json!({
            "command": [],
            "passed": true,
            "detail": "SUMMONER_LAND_ALLOW_NO_AGGREGATE set; aggregate gate skipped",
        }));
    }
    bail!(
        "land refuses to advance the protected branch without an aggregate verify: set SUMMONER_LAND_VERIFY to an argv, add a root Cargo.toml (cargo test), or set SUMMONER_LAND_ALLOW_NO_AGGREGATE=1 for an explicit no-gate landing"
    )
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
    aggregate: Option<Value>,
    integration_candidate: Option<Value>,
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
            "aggregate": aggregate,
            "integration_candidate": integration_candidate,
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
