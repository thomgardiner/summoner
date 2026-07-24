//! `summoner land`: integrate verified candidate commits onto the target branch.

mod aggregate;
mod git;
mod merge;
mod plan;
mod report;
mod seal;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

use git::{canonical, git};
use merge::merge_candidates;
use plan::{candidates, plan_landing};
use report::{bind_integration_envelope, latest_finished_run, report_result};
use seal::{abandon_integration, advance_to_integration, seal_and_gate};

pub(crate) struct Candidate {
    id: String,
    outcome: String,
    commit: Option<String>,
    after: Vec<String>,
}

/// The decided plan: what to merge and in what order, and what was set aside.
pub(crate) struct Plan {
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
        report_result(&repo, &run_dir, &plan, &[], None, None, None, dry_run)?;
        return Ok(0);
    }
    if dry_run {
        report_result(&repo, &run_dir, &plan, &[], None, None, None, true)?;
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
            &run_dir,
            &plan,
            &landed,
            stopped,
            aggregate,
            integration_candidate,
            false,
        )?;
        return Ok(1);
    }

    // Bind sealed I into the envelope *before* FF so a crash between advance
    // and report cannot leave the protected tip at I without land evidence.
    bind_integration_envelope(&run_dir, integration_candidate.as_ref())?;
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
        &run_dir,
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

#[cfg(test)]
mod tests {
    use super::plan::plan_landing;
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
