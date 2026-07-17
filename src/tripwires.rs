//! Deterministic anti-reward-hacking scan of an order's diff, run before any
//! LLM review. Protected paths are files whose modification makes the
//! verification evidence itself untrustworthy (grove reads profiles from the
//! worktree), so touching one caps the outcome at `unverified` outright. Soft
//! flags are suspicious-but-sometimes-legitimate patterns handed to the
//! reviewer to confirm or refute — the "monitor the process, not just the
//! outcome" half of the gate.

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
pub fn scan(worktree: &Path, base: &str) -> Tripwires {
    let mut changed: Vec<(char, String)> = Vec::new();
    for line in git(worktree, &["diff", "--name-status", base]).lines() {
        // "D\tpath" — renames ("R100\told\tnew") report the destination.
        let mut parts = line.split('\t');
        if let (Some(status), Some(path)) = (parts.next(), parts.next_back())
            && let Some(kind) = status.chars().next()
        {
            changed.push((kind, path.to_string()));
        }
    }
    for line in git(worktree, &["status", "--porcelain"]).lines() {
        if line.len() > 3 {
            let kind = if line.starts_with("??") {
                'A'
            } else {
                line.trim_start().chars().next().unwrap_or('M')
            };
            changed.push((kind, line[3..].trim().to_string()));
        }
    }
    let diff = git(worktree, &["diff", base]);
    analyze(&changed, &diff)
}

/// Pure analysis over a change list and unified diff text.
pub fn analyze(changed: &[(char, String)], diff: &str) -> Tripwires {
    let mut protected = BTreeSet::new();
    let mut flags = Vec::new();

    for (status, path) in changed {
        if PROTECTED.contains(&path.as_str()) {
            protected.insert(path.clone());
        }
        if *status == 'D' && path.contains("test") {
            flags.push(format!("deleted test file: {path}"));
        }
    }
    for path in &protected {
        flags.insert(0, format!("protected file modified: {path}"));
    }

    let added: Vec<&str> = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .collect();
    let removed: Vec<&str> = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .collect();

    let skips = added
        .iter()
        .filter(|l| SKIP_MARKERS.iter().any(|m| l.contains(m)))
        .count();
    if skips > 0 {
        flags.push(format!("test-skip marker(s) added: {skips}"));
    }

    let asserts_added = added.iter().filter(|l| l.contains("assert")).count();
    let asserts_removed = removed.iter().filter(|l| l.contains("assert")).count();
    if asserts_removed > asserts_added {
        flags.push(format!(
            "net assertion loss: {}",
            asserts_removed - asserts_added
        ));
    }

    if added.iter().any(|l| l.contains("[profile")) {
        flags.push("build profile section modified".to_string());
    }

    Tripwires {
        protected: protected.into_iter().collect(),
        flags,
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
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
    fn skip_markers_assertion_loss_and_profiles_are_counted_from_added_lines() {
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
}
