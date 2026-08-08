use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("running git rev-parse --show-toplevel")?;
    if !output.status.success() {
        bail!("current directory is not inside a git repository");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

pub fn head(repo: &Path) -> Result<String> {
    text(repo, &["rev-parse", "HEAD"])
}

pub fn resolve_commit(repo: &Path, reference: &str) -> Result<String> {
    if reference.trim().is_empty() {
        bail!("base reference must not be empty");
    }
    if reference.starts_with('-') {
        bail!("base reference must not begin with '-'");
    }
    text(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
}

pub fn branch_tip(repo: &Path, branch: &str) -> Result<String> {
    text(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("refs/heads/{branch}^{{commit}}"),
        ],
    )
}

pub fn current_branch(worktree: &Path) -> Result<String> {
    text(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

pub fn common_dir(worktree: &Path) -> Result<PathBuf> {
    let path = PathBuf::from(text(worktree, &["rev-parse", "--git-common-dir"])?);
    let path = if path.is_absolute() {
        path
    } else {
        worktree.join(path)
    };
    path.canonicalize()
        .with_context(|| format!("resolving Git common directory {}", path.display()))
}

pub fn assert_candidate(repo: &Path, worktree: &Path, branch: &str, candidate: &str) -> Result<()> {
    if dirty(worktree)? {
        bail!("worktree is dirty");
    }
    let actual_head = head(worktree)?;
    if actual_head != candidate {
        bail!("worktree HEAD moved from {candidate} to {actual_head}");
    }
    let actual_branch = current_branch(worktree)?;
    if actual_branch != branch {
        bail!("worktree is on {actual_branch}, expected {branch}");
    }
    let actual_tip = branch_tip(repo, branch)?;
    if actual_tip != candidate {
        bail!("branch {branch} moved from {candidate} to {actual_tip}");
    }
    Ok(())
}

pub fn text(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn add_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            branch,
            &path.to_string_lossy(),
            base,
        ])
        .current_dir(repo)
        .output()
        .context("git worktree add")?;
    if !output.status.success() {
        bail!(
            "git worktree add {branch} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["worktree", "remove", "--force", &path.to_string_lossy()])
        .current_dir(repo)
        .output()
        .context("git worktree remove")?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn dirty(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .context("git status")?;
    if !output.status.success() {
        bail!("git status failed");
    }
    Ok(!output.stdout.is_empty())
}

pub fn changed_paths(path: &Path, base: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let range = format!("{base}..HEAD");
    for args in [
        vec!["diff", "--no-renames", "--name-only", "-z", range.as_str()],
        vec!["diff", "--no-renames", "--name-only", "-z"],
        vec!["diff", "--no-renames", "--cached", "--name-only", "-z"],
        vec!["ls-files", "--others", "--exclude-standard", "-z"],
    ] {
        let output = Command::new("git")
            .args(&args)
            .current_dir(path)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        if !output.status.success() {
            bail!("git {} failed", args.join(" "));
        }
        paths.extend(
            output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).replace('\\', "/")),
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub fn salvage(path: &Path, message: &str) -> Result<bool> {
    if !dirty(path)? {
        return Ok(false);
    }
    let add = Command::new("git")
        .args(["add", "-A"])
        .current_dir(path)
        .output()
        .context("git add")?;
    if !add.status.success() {
        bail!(
            "git add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }
    let commit = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .context("git commit")?;
    if !commit.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(true)
}
