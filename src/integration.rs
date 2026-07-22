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
    /// A dependency finished without an immutable candidate commit (it left
    /// uncommitted work, or predates candidate recording), so there is nothing
    /// safe for this order to build on. Starting anyway would run the executor
    /// on a tree silently missing part of its declared inputs.
    MissingCandidate { id: String },
    /// The declared base does not contain a dependency's candidate, so the
    /// order would wait for work it then builds without.
    ExcludedDependency { id: String, base: String },
}

impl Base {
    /// What to pass to `grove worktree acquire --base`.
    pub fn commit(&self) -> Option<&str> {
        match self {
            Base::Declared(base) => base.as_deref(),
            Base::Inherited { commit, .. } | Base::Merged { commit, .. } => Some(commit),
            Base::Conflicted { .. } | Base::MissingCandidate { .. } => None,
            Base::ExcludedDependency { .. } => None,
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
            Base::MissingCandidate { id } => Some(format!(
                "dependency {id:?} finished without an immutable candidate commit \
                 (uncommitted work, or a run recorded before candidate commits); \
                 nothing safe to build on"
            )),
            Base::ExcludedDependency { id, base } => Some(format!(
                "declared base {base:?} does not contain dependency {id:?}'s \
                 candidate; the order would wait for work it then builds without"
            )),
        }
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// Resolve the base for `order`, given the verified commit of each finished
/// dependency (the scheduler dispatches only after every dependency reached a
/// satisfying outcome, so an id absent from `landed` finished WITHOUT a
/// candidate commit — it is not merely unfinished).
///
/// An explicit `base` wins, but only after proving it contains every
/// dependency's candidate: `after` is a dataflow edge, and a base that
/// excludes a dependency turns it back into mere ordering silently.
pub fn resolve(
    repo: &Path,
    explicit: Option<&str>,
    after: &[String],
    landed: &BTreeMap<String, String>,
) -> Result<Base> {
    if let Some(base) = explicit {
        // Unresolvable here just means the ref is not visible yet; worktree
        // acquisition is the authority on whether it exists at all. Only this
        // order's own dependencies are checked, and only those that recorded a
        // candidate: with an explicit base, a dependency without one is a pure
        // ordering edge, which is exactly the legacy contract.
        if let Ok(base_commit) = rev_parse(repo, base) {
            for id in after {
                let Some(commit) = landed.get(id) else {
                    continue;
                };
                if !is_ancestor(repo, commit, &base_commit)? {
                    return Ok(Base::ExcludedDependency {
                        id: id.clone(),
                        base: base.to_string(),
                    });
                }
            }
        }
        return Ok(Base::Declared(Some(base.to_string())));
    }
    let mut inherited: Vec<(&String, &String)> = Vec::new();
    for id in after {
        match landed.get(id) {
            Some(commit) => inherited.push((id, commit)),
            None => return Ok(Base::MissingCandidate { id: id.clone() }),
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

fn rev_parse(repo: &Path, reference: &str) -> Result<String> {
    let output = git(
        repo,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )?;
    if !output.status.success() {
        bail!("unresolvable reference {reference:?}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = git(repo, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    Ok(output.status.success())
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
