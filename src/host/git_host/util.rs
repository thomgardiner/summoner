//! Shared helpers for the git host.

use crate::host::git_ledger::TaskRecord;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Paths changed since task begin (commits + dirty tree) that leave scope.
pub(crate) fn outside_scope_paths(worktree: &Path, rec: &TaskRecord) -> Result<Vec<String>> {
    let mut outside = Vec::new();
    let range = format!("{}..HEAD", rec.start_commit);
    if let Ok(changed) = git_out(worktree, &["diff", "--name-only", &range]) {
        for line in changed.lines() {
            if line.is_empty() {
                continue;
            }
            if !in_scope(line, &rec.scope) {
                outside.push(line.to_string());
            }
        }
    }
    if let Ok(status) = git_out(worktree, &["status", "--porcelain"]) {
        for line in status.lines() {
            let path = line.get(3..).unwrap_or("").trim();
            if path.is_empty() {
                continue;
            }
            let path = path.split(" -> ").last().unwrap_or(path);
            if !in_scope(path, &rec.scope) {
                outside.push(path.to_string());
            }
        }
    }
    outside.sort();
    outside.dedup();
    Ok(outside)
}

pub(crate) fn runs_parent() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("summoner");
    }
    dirs_fallback().join("summoner")
}

pub(crate) fn dirs_fallback() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache")
}

pub(crate) fn repo_slug(repo: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(repo.to_string_lossy().as_bytes());
    let dig = h.finalize();
    dig.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn git_out(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn in_scope(path: &str, scope: &[String]) -> bool {
    scope.iter().any(|s| {
        path == s || path.starts_with(&format!("{s}/")) || s.starts_with("crate:") // opaque for git host
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Git-host exact-state requires a clean worktree: no staged, unstaged, or
/// untracked changes. Only then does binding HEAD equal the bytes verify saw.
pub(crate) fn require_clean_candidate(worktree: &Path) -> Result<()> {
    let status = git_out(worktree, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        bail!(
            "worktree is dirty; git host only verifies/finishes clean committed candidates. Commit or discard local changes first"
        );
    }
    Ok(())
}

/// Digest used for finish CAS and review binding: sha256 of `commit\0tree`
/// so both object ids must match. Only meaningful on a clean worktree.
pub(crate) fn candidate_source_digest(worktree: &Path) -> Result<String> {
    let commit = git_out(worktree, &["rev-parse", "HEAD"])?;
    let tree = git_out(worktree, &["rev-parse", "HEAD^{tree}"])?;
    Ok(sha256_hex(format!("{commit}\0{tree}").as_bytes()))
}
