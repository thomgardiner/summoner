//! Deterministic anti-reward-hacking scan of an order's diff, run before any
//! LLM review. Protected paths are files whose modification makes the
//! verification evidence itself untrustworthy (grove reads profiles from the
//! worktree), so touching one caps the outcome at `unverified` outright. Soft
//! flags are suspicious-but-sometimes-legitimate patterns handed to the
//! reviewer to confirm or refute — the "monitor the process, not just the
//! outcome" half of the gate. A scan that cannot collect evidence fails the
//! order rather than reporting a clean pass: this gate fails closed.

use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Files that configure verification or the build contract. The worker
/// charter forbids touching them; a diff that does anyway is either a
/// misbehaving executor or an attempt to weaken its own gate.
const PROTECTED: &[&str] = &[
    ".grove.toml",
    ".summoner.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config",
    ".cargo/config.toml",
    // Judge / meta-verification authority (always on, not only under trusted policy).
    ".crucible",
    "Cargo.lock",
];

/// Added-line markers that disable tests across the ecosystems we meet.
const SKIP_MARKERS: &[&str] = &[
    "#[ignore",
    ".skip(",
    "it.skip",
    "xit(",
    "xdescribe(",
    "@pytest.mark.skip",
    "@unittest.skip",
];

pub struct Tripwires {
    /// Protected paths the diff touched; non-empty means receipts are suspect.
    pub protected: Vec<String>,
    /// Human-readable findings, protected entries included, for the report
    /// and the review prompt.
    pub flags: Vec<String>,
}

/// Scan the committed and uncommitted delta since `base` in `worktree`.
/// `extra_protected` carries the trusted policy's `protected_paths`, which join
/// the built-in list: verification commands can read files Grove's policy digest
/// cannot bind (a `ci/verify.sh` the profile shells out to), so the operator
/// names them here and a diff that touches one caps the order at `unverified`.
pub fn scan(worktree: &Path, base: &str, extra_protected: &[String]) -> Result<Tripwires> {
    let mut changed = changed_entries(&git(worktree, &["diff", "--name-status", base])?);
    for line in git(worktree, &["status", "--porcelain"])?.lines() {
        if line.len() > 3 {
            let kind = if line.starts_with("??") {
                'A'
            } else {
                line.trim_start().chars().next().unwrap_or('M')
            };
            // Staged renames read "old -> new"; both sides count.
            for path in line[3..].split(" -> ") {
                changed.push((kind, path.trim().to_string()));
            }
        }
    }
    let diff = git(worktree, &["diff", base])?;
    Ok(analyze_with(&changed, &diff, extra_protected))
}

/// Parse `git diff --name-status` records. Renames and copies ("R100\told\tnew")
/// contribute BOTH sides: renaming a protected file away is still a
/// modification of the protected path.
fn changed_entries(name_status: &str) -> Vec<(char, String)> {
    let mut changed = Vec::new();
    for line in name_status.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next().and_then(|s| s.chars().next()) else {
            continue;
        };
        for path in parts {
            changed.push((status, path.to_string()));
        }
    }
    changed
}

/// Pure analysis over a change list and unified diff text.
#[cfg(test)]
pub fn analyze(changed: &[(char, String)], diff: &str) -> Tripwires {
    analyze_with(changed, diff, &[])
}

pub fn analyze_with(
    changed: &[(char, String)],
    diff: &str,
    extra_protected: &[String],
) -> Tripwires {
    let mut protected = BTreeSet::new();
    let mut flags = Vec::new();

    for (status, path) in changed {
        if PROTECTED.contains(&path.as_str())
            || extra_protected.iter().any(|entry| protects(entry, path))
        {
            protected.insert(path.clone());
        }
        if *status == 'D' && path.contains("test") {
            flags.push(format!("deleted test file: {path}"));
        }
    }
    for path in &protected {
        flags.insert(0, format!("protected file modified: {path}"));
    }

    let mut current_file = String::new();
    let mut skips = 0;
    let mut asserts_added = 0usize;
    let mut asserts_removed = 0usize;
    let mut profile_edit = false;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = path.to_string();
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if SKIP_MARKERS.iter().any(|marker| line.contains(marker)) {
                skips += 1;
            }
            if line.contains("assert") {
                asserts_added += 1;
            }
            // Only a manifest can change the build contract; "[profile" in
            // docs or fixtures is prose, not policy.
            if line.contains("[profile") && current_file.ends_with("Cargo.toml") {
                profile_edit = true;
            }
        } else if line.starts_with('-') && !line.starts_with("---") && line.contains("assert") {
            asserts_removed += 1;
        }
    }
    if skips > 0 {
        flags.push(format!("test-skip marker(s) added: {skips}"));
    }
    if asserts_removed > asserts_added {
        flags.push(format!(
            "net assertion loss: {}",
            asserts_removed - asserts_added
        ));
    }
    if profile_edit {
        flags.push("build profile section modified".to_string());
    }

    Tripwires {
        protected: protected.into_iter().collect(),
        flags,
    }
}

/// A policy entry protects an exact repo-relative path, or every path beneath it
/// when it names a directory. Paths are compared with forward slashes because
/// that is what git reports on every platform.
fn protects(entry: &str, path: &str) -> bool {
    let entry = entry.trim_end_matches('/');
    if entry.is_empty() {
        return false;
    }
    path == entry
        || path
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawning git {args:?} for the tripwire scan"))?;
    if !output.status.success() {
        bail!(
            "tripwire scan git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(changed: &[(char, &str)]) -> Vec<(char, String)> {
        changed
            .iter()
            .map(|(kind, path)| (*kind, path.to_string()))
            .collect()
    }

    #[test]
    fn protected_paths_and_deleted_tests_are_flagged() {
        let trips = analyze(
            &owned(&[
                ('M', ".grove.toml"),
                ('D', "tests/claim.rs"),
                ('M', "src/lib.rs"),
            ]),
            "",
        );
        assert_eq!(trips.protected, [".grove.toml"]);
        assert!(
            trips
                .flags
                .iter()
                .any(|f| f == "protected file modified: .grove.toml"),
            "{:?}",
            trips.flags
        );
        assert!(
            trips
                .flags
                .iter()
                .any(|f| f == "deleted test file: tests/claim.rs"),
            "{:?}",
            trips.flags
        );
    }

    #[test]
    fn renaming_a_protected_file_away_still_trips() {
        let changed = changed_entries("R100\t.cargo/config.toml\tdocs/archive.toml\nM\tsrc/a.rs");
        let trips = analyze(&changed, "");
        assert_eq!(trips.protected, [".cargo/config.toml"]);
    }

    #[test]
    fn skip_markers_assertion_loss_and_manifest_profiles_are_counted() {
        let diff = "\
--- a/tests/t.rs
+++ b/tests/t.rs
-    assert_eq!(total, 7);
-    assert!(ok);
+    #[ignore]
+++ b/Cargo.toml
+[profile.release]
";
        let trips = analyze(&[], diff);
        assert!(
            trips
                .flags
                .iter()
                .any(|f| f == "test-skip marker(s) added: 1"),
            "{:?}",
            trips.flags
        );
        assert!(
            trips.flags.iter().any(|f| f == "net assertion loss: 2"),
            "{:?}",
            trips.flags
        );
        assert!(
            trips
                .flags
                .iter()
                .any(|f| f == "build profile section modified"),
            "{:?}",
            trips.flags
        );
        assert!(trips.protected.is_empty());
    }

    #[test]
    fn profile_text_outside_a_manifest_is_prose_not_policy() {
        let diff = "\
+++ b/docs/tuning.md
+add [profile.release] to your Cargo.toml
";
        let trips = analyze(&[], diff);
        assert!(trips.flags.is_empty(), "{:?}", trips.flags);
    }

    #[test]
    fn a_clean_feature_diff_raises_nothing() {
        let diff = "\
+++ b/src/lib.rs
+pub fn wave() {}
+    assert!(wave_exists());
";
        let trips = analyze(&owned(&[('M', "src/lib.rs")]), diff);
        assert!(trips.protected.is_empty());
        assert!(trips.flags.is_empty(), "{:?}", trips.flags);
    }

    #[test]
    fn policy_protected_paths_join_the_builtin_list_exactly_and_by_directory() {
        let policy = ["ci/verify.sh".to_string(), "scripts/gates".to_string()];
        let trips = analyze_with(
            &owned(&[
                ('M', "ci/verify.sh"),
                ('M', "scripts/gates/lint.sh"),
                ('M', "src/lib.rs"),
                ('M', "ci/verify.sh.bak"),
                ('M', "scripts/gates-extra/x.sh"),
            ]),
            "",
            &policy,
        );
        assert_eq!(trips.protected, ["ci/verify.sh", "scripts/gates/lint.sh"]);
        assert!(
            trips
                .flags
                .iter()
                .any(|f| f == "protected file modified: ci/verify.sh"),
            "{:?}",
            trips.flags
        );
    }

    #[test]
    fn an_empty_policy_entry_protects_nothing() {
        let trips = analyze_with(
            &owned(&[('M', "src/lib.rs")]),
            "",
            &["".to_string(), "/".to_string()],
        );
        assert!(trips.protected.is_empty(), "{:?}", trips.protected);
    }
}
