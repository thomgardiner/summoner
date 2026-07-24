//! Host conformance invariants for assurance identity (I1, I5, I7).
//!
//! These tests document machine-checkable contracts without requiring a live
//! model CLI. They exercise land's integration seal and the assurance envelope
//! composition path.

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
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "c@example.invalid"]);
    git(repo, &["config", "user.name", "Conformance"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
}

fn candidate(repo: &Path, branch: &str, file: &str, content: &str) -> String {
    git(repo, &["checkout", "-q", "-b", branch]);
    std::fs::write(repo.join(file), content).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", branch]);
    let sha = git(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    sha
}

#[test]
fn land_ff_targets_exact_sealed_integration_commit() {
    let root = tempdir().unwrap();
    let repo = root.path().join("repo");
    let cache = root.path().join("cache");
    init(&repo);
    let a = candidate(&repo, "a", "a.txt", "a\n");
    let run_id = "2000000000-1";
    let run_dir = cache.join("summoner").join("runs").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    let report = json!({
        "repo": std::fs::canonicalize(&repo).unwrap().display().to_string(),
        "orders": [{"id": "a", "outcome": "verified", "candidate_commit": a, "after": []}],
    });
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();

    let out = Command::new(SUMMONER)
        .args(["land", run_id])
        .current_dir(&repo)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .env("SUMMONER_LAND_ALLOW_NO_AGGREGATE", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let land: Value = serde_json::from_slice(&out.stdout).unwrap();
    let i = &land["integration_candidate"];
    assert_eq!(i["integration_commit"], a);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), a);
    assert_eq!(
        git(&repo, &["rev-parse", i["retained_ref"].as_str().unwrap()]),
        a
    );
}

#[test]
fn crucible_gate_failure_blocks_land_ff() {
    let root = tempdir().unwrap();
    let repo = root.path().join("repo");
    let cache = root.path().join("cache");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // Fake crucible that always fails check.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join("crucible");
        std::fs::write(&path, "#!/bin/sh\necho fail >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        std::fs::write(
            bin.join("crucible.CMD"),
            "@echo off\r\necho fail 1>&2\r\nexit /b 1\r\n",
        )
        .unwrap();
    }
    init(&repo);
    let a = candidate(&repo, "a", "a.txt", "a\n");
    let run_id = "2000000000-3";
    let run_dir = cache.join("summoner").join("runs").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_vec_pretty(&json!({
            "repo": std::fs::canonicalize(&repo).unwrap().display().to_string(),
            "orders": [{"id": "a", "outcome": "verified", "candidate_commit": a, "after": []}],
        }))
        .unwrap(),
    )
    .unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = Command::new(SUMMONER)
        .args(["land", run_id])
        .current_dir(&repo)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .env("PATH", path)
        .env("SUMMONER_LAND_ALLOW_NO_AGGREGATE", "1")
        .env("SUMMONER_LAND_CRUCIBLE", "check")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let land: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(land["stopped"]["id"], "_gate");
    assert!(
        land["stopped"]["reason"]
            .as_str()
            .unwrap_or("")
            .contains("crucible"),
        "{land}"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
}

#[test]
fn dirty_misidentified_candidate_is_refused_by_land_when_missing_object() {
    let root = tempdir().unwrap();
    let repo = root.path().join("repo");
    let cache = root.path().join("cache");
    init(&repo);
    let run_id = "2000000000-2";
    let run_dir = cache.join("summoner").join("runs").join(run_id);
    std::fs::create_dir_all(&run_dir).unwrap();
    let fake = "0123456789abcdef0123456789abcdef01234567";
    let report = json!({
        "repo": std::fs::canonicalize(&repo).unwrap().display().to_string(),
        "orders": [{"id": "ghost", "outcome": "verified", "candidate_commit": fake, "after": []}],
    });
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let out = Command::new(SUMMONER)
        .args(["land", run_id])
        .current_dir(&repo)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .env("SUMMONER_LAND_ALLOW_NO_AGGREGATE", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let land: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(land["stopped"]["id"], "ghost");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
}
