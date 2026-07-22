//! What a dependent order builds from.
//!
//! `after` only orders work: without help, an order that declares a dependency
//! still branches from the repository default, so the dependency's code is not
//! present and the user has to hand-write `base = "grove/smn-<dep>"`. That is a
//! footgun in a fleet, because the deterministic branch name is not the same
//! thing as the commit that was actually verified: releasing a worktree can
//! salvage dirty state into a new commit and move the branch past it.
//!
//! So a dependent inherits its dependencies' `candidate_commit`s, which name
//! exactly what passed verification and review. One dependency is inherited
//! directly. Several are merged, sequentially and in declared order, with
//! `git merge-tree`, which computes the merge without a worktree and reports
//! conflicts instead of leaving a half-merged index anywhere.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// The commit a dependent should branch from, and why.
#[derive(Debug, PartialEq, Eq)]
pub enum Base {
    /// No dependencies, or the order named its own base: leave it alone.
    Declared(Option<String>),
    /// Exactly one dependency; build directly on its verified commit.
    Inherited { from: String, commit: String },
    /// Several dependencies merged into one commit made for this order.
    Merged { from: Vec<String>, commit: String },
    /// The dependencies do not combine; the order cannot start.
    Conflicted {
        left: String,
        right: String,
        paths: Vec<String>,
    },
}

impl Base {
    /// What to pass to `grove worktree acquire --base`.
    pub fn commit(&self) -> Option<&str> {
        match self {
            Base::Declared(base) => base.as_deref(),
            Base::Inherited { commit, .. } | Base::Merged { commit, .. } => Some(commit),
            Base::Conflicted { .. } => None,
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            Base::Declared(_) => None,
            Base::Inherited { from, commit } => {
                Some(format!("built on {from} at {}", short(commit)))
            }
            Base::Merged { from, commit } => Some(format!(
                "built on merged {} at {}",
                from.join(" + "),
                short(commit)
            )),
            Base::Conflicted { left, right, paths } => Some(format!(
                "dependencies {left} and {right} conflict in {}; \
                 land them in one order or give this order an explicit base",
                paths.join(", ")
            )),
        }
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Resolve the base for `order`, given the verified commit of each finished
/// dependency. An explicit `base` always wins: the orchestrator asked for it.
pub fn resolve(
    repo: &Path,
    explicit: Option<&str>,
    after: &[String],
    landed: &BTreeMap<String, String>,
) -> Result<Base> {
    if let Some(base) = explicit {
        return Ok(Base::Declared(Some(base.to_string())));
    }
    // A dependency with no recorded commit contributed no code to build on
    // (it may have been carried, or produced nothing); skip rather than fail.
    let mut inherited: Vec<(&String, &String)> = Vec::new();
    for id in after {
        if let Some(commit) = landed.get(id) {
            inherited.push((id, commit));
        }
    }
    match inherited.as_slice() {
        [] => Ok(Base::Declared(None)),
        [(id, commit)] => Ok(Base::Inherited {
            from: (*id).clone(),
            commit: (*commit).clone(),
        }),
        many => {
            let mut accumulated = many[0].1.clone();
            let mut names = vec![many[0].0.clone()];
            for (id, commit) in &many[1..] {
                match merge(repo, &accumulated, commit, &names, id)? {
                    Ok(merged) => {
                        accumulated = merged;
                        names.push((*id).clone());
                    }
                    Err(paths) => {
                        return Ok(Base::Conflicted {
                            left: names.join(" + "),
                            right: (*id).clone(),
                            paths,
                        });
                    }
                }
            }
            Ok(Base::Merged {
                from: names,
                commit: accumulated,
            })
        }
    }
}

/// Merge two commits without touching a worktree. `Ok(oid)` is a clean merge;
/// `Err(paths)` lists the conflicted paths.
fn merge(
    repo: &Path,
    left: &str,
    right: &str,
    left_names: &[String],
    right_name: &str,
) -> Result<std::result::Result<String, Vec<String>>> {
    let output = git(repo, &["merge-tree", "--write-tree", left, right])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let tree = lines
        .next()
        .map(str::trim)
        .filter(|tree| !tree.is_empty())
        .context("git merge-tree wrote no tree")?
        .to_string();
    if !output.status.success() {
        // Conflicted entries follow the tree as `<mode> <oid> <stage>\t<path>`.
        let mut paths: Vec<String> = lines
            .filter_map(|line| line.split_once('\t').map(|(_, path)| path.to_string()))
            .collect();
        paths.sort();
        paths.dedup();
        return Ok(Err(paths));
    }
    let message = format!(
        "summoner: integrate {} into {}",
        right_name,
        left_names.join(" + ")
    );
    let commit = git(
        repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            left,
            "-p",
            right,
            "-m",
            &message,
        ],
    )?;
    if !commit.status.success() {
        bail!(
            "git commit-tree failed: {}",
            String::from_utf8_lossy(&commit.stderr).trim()
        );
    }
    Ok(Ok(String::from_utf8_lossy(&commit.stdout)
        .trim()
        .to_string()))
}

fn git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("running git {args:?}"))
}

#[cfg(test)]
#[path = "integration_tests.rs"]
mod tests;
