use crate::{git, order, report};
use anyhow::Result;
use std::path::Path;

pub fn capture(
    order: &order::Order,
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    result: &mut report::OrderReport,
) -> Result<bool> {
    let scopes = order::normalized_scopes(order)?;
    let changed = git::changed_paths(worktree, base)?;
    let outside: Vec<_> = changed
        .iter()
        .filter(|path| !order::in_scope(path, &scopes))
        .cloned()
        .collect();
    if !outside.is_empty() {
        result.outcome = "scope_violation".into();
        result.changed_paths = changed;
        result.detail = Some(format!(
            "changed paths outside scope: {}; worktree kept",
            outside.join(", ")
        ));
        return Ok(false);
    }
    if let Err(error) = git::salvage(worktree, &format!("summoner: salvage {}", order.id)) {
        result.outcome = "error".into();
        result.changed_paths = changed;
        result.detail = Some(format!("worktree kept: {error}"));
        return Ok(false);
    }
    result.changed_paths = git::changed_paths(worktree, base)?;
    if !result.changed_paths.is_empty() {
        result.candidate_commit = Some(git::head(worktree)?);
    }
    let candidate = result.candidate_commit.as_deref().unwrap_or(base);
    git::assert_candidate(repo, worktree, branch, candidate)?;
    Ok(true)
}

pub fn cleanup(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    candidate: &str,
    result: &mut report::OrderReport,
) -> bool {
    if let Err(error) = git::assert_candidate(repo, worktree, branch, candidate) {
        append_detail(result, format!("worktree kept: {error}"));
        return false;
    }
    match git::remove_worktree(repo, worktree) {
        Ok(()) => {
            result.worktree = None;
            true
        }
        Err(error) => {
            append_detail(result, format!("worktree kept: {error}"));
            false
        }
    }
}

fn append_detail(result: &mut report::OrderReport, detail: String) {
    result.detail = Some(match result.detail.take() {
        Some(existing) if !existing.is_empty() => format!("{existing}; {detail}"),
        _ => detail,
    });
}
