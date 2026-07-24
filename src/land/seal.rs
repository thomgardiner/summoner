//! Seal integration candidate I and run land gates.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

use super::aggregate::aggregate_verify;
use super::git::git;

pub(crate) fn seal_and_gate(
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
    match run_integration_gates(repo, base_commit, &sealed) {
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
                "id": "_gate",
                "reason": format!("{error:#}"),
            })),
        ),
    }
}

/// Aggregate verify, optional Crucible arms, optional holder review — all against I.
pub(crate) fn run_integration_gates(
    repo: &Path,
    base_commit: &str,
    integration_commit: &str,
) -> Result<Value> {
    let mut report = aggregate_verify(repo)?;
    if let Some(crucible) = crucible_gate(repo, base_commit, integration_commit)? {
        report["crucible"] = crucible;
    }
    if let Some(review) = land_review_gate(repo, base_commit, integration_commit)? {
        report["holder_review"] = review;
    }
    assert_still_at(repo, integration_commit)?;
    Ok(report)
}

/// `SUMMONER_LAND_CRUCIBLE=check` or comma arms `check,harden` (requires `crucible` on PATH).
pub(crate) fn crucible_gate(repo: &Path, base: &str, candidate: &str) -> Result<Option<Value>> {
    let Ok(raw) = std::env::var("SUMMONER_LAND_CRUCIBLE") else {
        return Ok(None);
    };
    let arms: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if arms.is_empty() {
        bail!("SUMMONER_LAND_CRUCIBLE is empty");
    }
    let bin = std::env::var("SUMMONER_CRUCIBLE_BIN").unwrap_or_else(|_| "crucible".into());
    let mut results = Vec::new();
    for arm in arms {
        let mut cmd = Command::new(&bin);
        cmd.current_dir(repo);
        match arm {
            "check" => {
                cmd.arg("check");
            }
            "harden" => {
                cmd.args(["harden", "--base", base, "--candidate", candidate]);
            }
            "run" => {
                cmd.arg("run");
            }
            other => bail!("unknown SUMMONER_LAND_CRUCIBLE arm {other:?} (check|harden|run)"),
        }
        let output = cmd
            .output()
            .with_context(|| format!("running {bin} {arm}"))?;
        let ok = output.status.success();
        results.push(json!({
            "arm": arm,
            "ok": ok,
            "exit": output.status.code(),
            "stderr_tail": String::from_utf8_lossy(&output.stderr).chars().rev().take(500).collect::<String>().chars().rev().collect::<String>(),
        }));
        if !ok {
            bail!(
                "crucible {arm} failed against integration candidate {candidate}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        assert_still_at(repo, candidate)?;
    }
    Ok(Some(json!({ "arms": results })))
}

/// Optional holder review against I: `SUMMONER_LAND_REVIEW` = shell-free argv
/// (`\x1f`-joined or whitespace). Runs with cwd = repo at I; must exit 0.
pub(crate) fn land_review_gate(repo: &Path, base: &str, candidate: &str) -> Result<Option<Value>> {
    let Ok(raw) = std::env::var("SUMMONER_LAND_REVIEW") else {
        return Ok(None);
    };
    let argv: Vec<&str> = if raw.contains('\u{1f}') {
        raw.split('\u{1f}').filter(|s| !s.is_empty()).collect()
    } else {
        raw.split_whitespace().collect()
    };
    if argv.is_empty() {
        bail!("SUMMONER_LAND_REVIEW is empty");
    }
    let output = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(repo)
        .env("SUMMONER_LAND_BASE", base)
        .env("SUMMONER_LAND_CANDIDATE", candidate)
        .output()
        .with_context(|| format!("running land holder review {}", argv[0]))?;
    if !output.status.success() {
        bail!(
            "holder review failed against integration candidate {candidate}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    assert_still_at(repo, candidate)?;
    Ok(Some(json!({
        "argv": argv,
        "ok": true,
        "base_commit": base,
        "integration_commit": candidate,
    })))
}

pub(crate) fn advance_to_integration(
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
pub(crate) fn capture_integration(
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
pub(crate) fn retain_integration(repo: &Path, captured: &mut Value) -> Result<()> {
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

pub(crate) fn abandon_integration(repo: &Path, target_branch: &str, integration_branch: &str) {
    let _ = git(repo, &["checkout", target_branch]);
    let _ = Command::new("git")
        .args(["branch", "-D", integration_branch])
        .current_dir(repo)
        .output();
}

pub(crate) fn assert_still_at(repo: &Path, expected: &str) -> Result<()> {
    let head = git(repo, &["rev-parse", "HEAD"])?;
    if head != expected {
        bail!("integration candidate drifted during gating: expected {expected}, HEAD is {head}");
    }
    Ok(())
}
