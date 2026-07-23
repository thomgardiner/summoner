//! `summoner land` against a real repository: merges a finished run's verified
//! candidate commits onto a temporary integration branch in dependency order,
//! then fast-forwards the protected target only if the whole set merges cleanly.

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "land@example.invalid"]);
    git(repo, &["config", "user.name", "Land Test"]);
    write(repo, "base.txt", "base\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
}

fn write(repo: &Path, path: &str, content: &str) {
    std::fs::write(repo.join(path), content).unwrap();
}

/// Commit `content` to `file` on a fresh branch off `base_ref`, returning the
/// new commit sha. Leaves `main` checked out.
fn candidate(repo: &Path, branch: &str, base_ref: &str, file: &str, content: &str) -> String {
    git(repo, &["checkout", "-q", "-b", branch, base_ref]);
    write(repo, file, content);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", branch]);
    let sha = git(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    sha
}

/// Write a run's `report.json` under an isolated runs root and return
/// `(run_id, env for the runs root)`.
fn stage_run(cache: &Path, repo: &Path, orders: Value) -> String {
    let run_id = "1000000000-1".to_string();
    let run_dir = cache.join("summoner").join("runs").join(&run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    let report = json!({
        "repo": std::fs::canonicalize(repo).unwrap().display().to_string(),
        "orders": orders,
    });
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    run_id
}

fn land(repo: &Path, cache: &Path, run_id: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["land", run_id];
    args.extend_from_slice(extra);
    let output = Command::new(SUMMONER)
        .args(&args)
        .current_dir(repo)
        .env("XDG_CACHE_HOME", cache)
        .env("HOME", cache)
        .output()
        .expect("run summoner land");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "land stdout was not JSON: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap(), report)
}

#[test]
fn land_merges_verified_candidates_in_dependency_order() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);

    let a = candidate(&repo, "a", "main", "a.txt", "a\n");
    let b = candidate(&repo, "b", "a", "b.txt", "b\n");
    let run_id = stage_run(
        &cache,
        &repo,
        json!([
            {"id": "b", "outcome": "verified", "candidate_commit": b, "after": ["a"]},
            {"id": "a", "outcome": "verified", "candidate_commit": a, "after": []},
            {"id": "c", "outcome": "rejected", "after": []},
        ]),
    );

    let (code, report) = land(&repo, &cache, &run_id, &[]);
    assert_eq!(code, 0, "clean landing exits 0: {report}");
    let landed: Vec<&str> = report["landed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(landed, ["a", "b"], "deps land before dependents");
    assert!(report["stopped"].is_null());
    assert_eq!(report["skipped"][0]["id"], "c");

    // Both dependency commits are now integrated on main; the rejected one is not.
    assert!(repo.join("a.txt").exists());
    assert!(repo.join("b.txt").exists());
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        b,
        "a linear chain fast-forwards"
    );
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn land_stops_at_the_first_conflict_leaving_a_clean_tree() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    // Two independent candidates that edit the same file differently: the first
    // lands, the second cannot merge.
    write(&repo, "x.txt", "0\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "seed x"]);
    let p = candidate(&repo, "p", "main", "x.txt", "p\n");
    let q = candidate(&repo, "q", "main", "x.txt", "q\n");
    let run_id = stage_run(
        &cache,
        &repo,
        json!([
            {"id": "p", "outcome": "verified", "candidate_commit": p, "after": []},
            {"id": "q", "outcome": "approved", "candidate_commit": q, "after": []},
        ]),
    );

    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let (code, report) = land(&repo, &cache, &run_id, &[]);
    assert_eq!(code, 1, "a conflict is a domain refusal: {report}");
    assert_eq!(report["landed"].as_array().unwrap().len(), 1);
    assert_eq!(report["landed"][0]["id"], "p");
    assert_eq!(report["stopped"]["id"], "q");

    // Protected target is unchanged: partial integration must not land.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(std::fs::read_to_string(repo.join("x.txt")).unwrap(), "0\n");
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn dry_run_reports_the_plan_without_merging() {
    let base = tempdir().unwrap();
    let repo = base.path().join("repo");
    let cache = base.path().join("cache");
    init(&repo);
    let a = candidate(&repo, "a", "main", "a.txt", "a\n");
    let run_id = stage_run(
        &cache,
        &repo,
        json!([{"id": "a", "outcome": "verified", "candidate_commit": a, "after": []}]),
    );

    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let (code, report) = land(&repo, &cache, &run_id, &["--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["planned"][0], "a");
    assert!(report["landed"].as_array().unwrap().is_empty());
    // Nothing merged: main is untouched and a.txt never appeared.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert!(!repo.join("a.txt").exists());
}
