use super::*;
use std::process::Command as Cmd;

fn run(repo: &Path, args: &[&str]) -> String {
    let out = Cmd::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A base commit plus two independent children, and two children that both
/// rewrite the same file. Real git objects, because the whole point is that the
/// merge is computed by git rather than guessed.
fn repo() -> (tempfile::TempDir, BTreeMap<String, String>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    run(path, &["init", "-q", "-b", "main"]);
    run(path, &["config", "user.email", "t@example.invalid"]);
    run(path, &["config", "user.name", "Integration Test"]);
    std::fs::write(path.join("shared.txt"), "base\n").unwrap();
    run(path, &["add", "-A"]);
    run(path, &["commit", "-qm", "base"]);
    let base = run(path, &["rev-parse", "HEAD"]);

    let mut landed = BTreeMap::new();
    for (id, file) in [("alpha", "alpha.txt"), ("beta", "beta.txt")] {
        run(path, &["checkout", "-q", &base]);
        std::fs::write(path.join(file), format!("{id}\n")).unwrap();
        run(path, &["add", "-A"]);
        run(path, &["commit", "-qm", id]);
        landed.insert(id.to_string(), run(path, &["rev-parse", "HEAD"]));
    }
    // Two orders that rewrite the same file cannot be combined.
    for id in ["clash-left", "clash-right"] {
        run(path, &["checkout", "-q", &base]);
        std::fs::write(path.join("shared.txt"), format!("{id}\n")).unwrap();
        run(path, &["add", "-A"]);
        run(path, &["commit", "-qm", id]);
        landed.insert(id.to_string(), run(path, &["rev-parse", "HEAD"]));
    }
    run(path, &["checkout", "-q", "main"]);
    (dir, landed)
}

#[test]
fn an_explicit_base_is_never_overridden() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        Some("grove/smn-chosen"),
        &["alpha".to_string()],
        &landed,
    )
    .unwrap();
    assert_eq!(resolved, Base::Declared(Some("grove/smn-chosen".into())));
    assert_eq!(resolved.commit(), Some("grove/smn-chosen"));
}

#[test]
fn no_dependencies_leaves_the_base_alone() {
    let (dir, landed) = repo();
    let resolved = resolve(dir.path(), None, &[], &landed).unwrap();
    assert_eq!(resolved, Base::Declared(None));
    assert_eq!(resolved.commit(), None);
}

#[test]
fn one_dependency_is_inherited_directly_without_a_merge_commit() {
    let (dir, landed) = repo();
    let resolved = resolve(dir.path(), None, &["alpha".to_string()], &landed).unwrap();
    let Base::Inherited { from, commit } = &resolved else {
        panic!("expected inheritance, got {resolved:?}");
    };
    assert_eq!(from, "alpha");
    assert_eq!(commit, &landed["alpha"]);
}

#[test]
fn two_dependencies_merge_into_a_commit_containing_both() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        None,
        &["alpha".to_string(), "beta".to_string()],
        &landed,
    )
    .unwrap();
    let Base::Merged { from, commit } = &resolved else {
        panic!("expected a merge, got {resolved:?}");
    };
    assert_eq!(from, &["alpha".to_string(), "beta".to_string()]);

    // The merge is a real commit reachable from both dependencies, carrying
    // both files. Anything less would silently drop a dependency's work.
    let files = run(dir.path(), &["ls-tree", "--name-only", "-r", commit]);
    assert!(files.contains("alpha.txt"), "{files}");
    assert!(files.contains("beta.txt"), "{files}");
    for id in ["alpha", "beta"] {
        let merge_base = run(
            dir.path(),
            &["merge-base", "--is-ancestor", &landed[id], commit],
        );
        assert_eq!(merge_base, "", "{id} must be an ancestor of the merge");
    }
}

#[test]
fn dependencies_that_cannot_combine_are_reported_rather_than_silently_dropped() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        None,
        &["clash-left".to_string(), "clash-right".to_string()],
        &landed,
    )
    .unwrap();
    let Base::Conflicted { left, right, paths } = &resolved else {
        panic!("expected a conflict, got {resolved:?}");
    };
    assert_eq!(left, "clash-left");
    assert_eq!(right, "clash-right");
    assert_eq!(paths, &["shared.txt".to_string()]);
    // No base means the order must not start on a wrong tree.
    assert_eq!(resolved.commit(), None);
    assert!(resolved.detail().unwrap().contains("shared.txt"));
}

/// The scheduler dispatches only after every dependency finished, so a
/// dependency absent from `landed` finished WITHOUT a candidate commit.
/// Building on the remaining dependencies would silently drop an input; the
/// order must refuse instead.
#[test]
fn a_dependency_without_a_candidate_commit_refuses_the_order() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        None,
        &["ghost".to_string(), "alpha".to_string()],
        &landed,
    )
    .unwrap();
    let Base::MissingCandidate { id } = &resolved else {
        panic!("expected a fail-closed refusal, got {resolved:?}");
    };
    assert_eq!(id, "ghost");
    assert_eq!(resolved.commit(), None);
    assert!(resolved.detail().unwrap().contains("ghost"));
}

/// A missing candidate fails the order closed regardless of base: an explicit
/// base cannot be proven to contain work no commit identifies, so it must not
/// be a loophole around the implicit-base MissingCandidate refusal.
#[test]
fn a_missing_candidate_refuses_even_with_an_explicit_base() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        Some("main"),
        &["ghost".to_string(), "alpha".to_string()],
        &landed,
    )
    .unwrap();
    let Base::MissingCandidate { id } = &resolved else {
        panic!("expected fail-closed even with an explicit base, got {resolved:?}");
    };
    assert_eq!(id, "ghost");
    assert_eq!(resolved.commit(), None);
}

/// An explicit base only wins when it contains every dependency's candidate:
/// `after` is a dataflow edge, and a base that excludes a dependency would
/// wait for work and then build without it.
#[test]
fn an_explicit_base_that_excludes_a_dependency_refuses_the_order() {
    let (dir, landed) = repo();
    // main is the fixture's base commit; alpha's work is not on it.
    let resolved = resolve(dir.path(), Some("main"), &["alpha".to_string()], &landed).unwrap();
    let Base::ExcludedDependency { id, base } = &resolved else {
        panic!("expected an exclusion refusal, got {resolved:?}");
    };
    assert_eq!(id, "alpha");
    assert_eq!(base, "main");
    assert_eq!(resolved.commit(), None);
}

/// A base that DOES contain the dependency passes: the legacy pattern of
/// `after = ["a"], base = "grove/smn-a"` keeps working when the branch carries
/// the candidate.
#[test]
fn an_explicit_base_containing_the_dependency_is_accepted() {
    let (dir, landed) = repo();
    let alpha = landed["alpha"].clone();
    let resolved = resolve(dir.path(), Some(&alpha), &["alpha".to_string()], &landed).unwrap();
    assert_eq!(resolved, Base::Declared(Some(alpha)));
}

/// An unresolvable explicit base is not this resolver's call to reject:
/// worktree acquisition owns ref existence. It passes through unchanged.
#[test]
fn an_unresolvable_explicit_base_passes_through() {
    let (dir, landed) = repo();
    let resolved = resolve(
        dir.path(),
        Some("grove/smn-not-yet-created"),
        &["alpha".to_string()],
        &landed,
    )
    .unwrap();
    assert_eq!(
        resolved,
        Base::Declared(Some("grove/smn-not-yet-created".to_string()))
    );
}
