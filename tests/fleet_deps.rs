//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use std::process::Command;

#[test]
fn dependent_order_builds_on_its_dependency_branch() {
    require_grove();
    let fixture = Fixture::new(true);
    // The second order proves it saw the first order's work: it refuses to
    // proceed unless src/one.rs (committed by order one) is present.
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) echo 'pub fn one() {}' > src/one.rs ;;\n\
           *smn-two) test -f src/one.rs || exit 9\n\
                     echo 'pub fn two() {}' > src/two.rs ;;\n\
         esac\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    let a = fixture.order(
        "a.toml",
        r#"
id = "one"
title = "Touch file one"
brief = "Write src/one.rs and commit."
scope = ["src/one.rs"]
verify_profile = "fast"
"#,
    );
    let b = fixture.order(
        "b.toml",
        r#"
id = "two"
title = "Build on file one"
brief = "Write src/two.rs next to src/one.rs and commit."
scope = ["src/two.rs"]
verify_profile = "fast"
after = ["one"]
base = "grove/smn-one"
"#,
    );

    let report = fixture.run_report(&[&a, &b], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    let two = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "two")
        .unwrap();
    assert_eq!(two["after"], serde_json::json!(["one"]));
    // Both files exist on order two's branch: the chain composed.
    let show = Command::new("git")
        .args(["show", "grove/smn-two:src/one.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "src/one.rs missing on smn-two");
}

#[test]
fn dependents_of_a_failed_order_are_skipped() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) exit 3 ;;\n\
           *) echo 'pub fn two() {}' > src/two.rs\n\
              git add -A\ngit commit -qm 'executor work' ;;\n\
         esac",
        60,
    );
    let a = fixture.order(
        "a.toml",
        r#"
id = "one"
title = "Fail"
brief = "Exit 3."
scope = ["src/one.rs"]
"#,
    );
    let b = fixture.order(
        "b.toml",
        r#"
id = "two"
title = "Never runs"
brief = "Should be skipped."
scope = ["src/two.rs"]
after = ["one"]
"#,
    );

    let report = fixture.run_report(&[&a, &b], 1);
    assert_eq!(report["summary"]["executor_failed"], 1, "{report}");
    assert_eq!(report["summary"]["skipped"], 1, "{report}");
    let two = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "two")
        .unwrap();
    assert_eq!(two["outcome"], "skipped");
    assert!(
        two["detail"]
            .as_str()
            .is_some_and(|d| d.contains("\"one\"") && d.contains("executor_failed")),
        "{report}"
    );
    // The skipped order never began a task: only one grove task exists.
    assert_eq!(fixture.task_states().len(), 1);
}

#[test]
fn a_dependent_order_inherits_its_dependency_without_an_explicit_base() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-first) echo 'pub fn first() {}' > src/first.rs ;;\n\
           *smn-second) test -f src/first.rs || exit 9\n\
                        echo 'pub fn second() {}' > src/second.rs ;;\n\
         esac\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    let a = fixture.order(
        "first.toml",
        r#"
id = "first"
title = "Write first"
brief = "Write src/first.rs and commit."
scope = ["src/first.rs"]
verify_profile = "fast"
"#,
    );
    // Deliberately no `base`: summoner must derive it from `after`.
    let b = fixture.order(
        "second.toml",
        r#"
id = "second"
title = "Build on first"
brief = "Write src/second.rs next to src/first.rs and commit."
scope = ["src/second.rs"]
verify_profile = "fast"
after = ["first"]
"#,
    );

    let report = fixture.run_report(&[&a, &b], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    let orders = report["orders"].as_array().unwrap();
    let first = orders.iter().find(|o| o["id"] == "first").unwrap();
    let second = orders.iter().find(|o| o["id"] == "second").unwrap();

    // The dependent branched from the dependency's exact verified commit, not
    // from its branch name and not from the repository default.
    assert_eq!(
        second["base_commit"].as_str().unwrap(),
        first["candidate_commit"].as_str().unwrap(),
        "second must start at first's verified commit: {report}"
    );
    assert!(
        second["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("built on first"),
        "the inheritance must be recorded: {second}"
    );
}

/// Review-required regression: an upstream that leaves uncommitted work has no
/// immutable candidate, so it records no candidate commit and its dependent
/// refuses to start rather than building on a tree missing that work. HEAD
/// alone must never be presented as the identity of a dirty candidate.

#[test]
fn a_dirty_upstream_records_no_candidate_and_its_dependent_refuses() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-dirty) echo 'pub fn dirty() {}' > src/one.rs ;;\n\
           *) echo 'pub fn other() {}' > src/two.rs\n\
              git add -A\ngit commit -qm 'executor work' ;;\n\
         esac",
        60,
    );
    let a = fixture.order(
        "dirty.toml",
        r#"
id = "dirty"
title = "Leave uncommitted work"
brief = "Write src/one.rs and do not commit."
scope = ["src/one.rs"]
verify_profile = "fast"
"#,
    );
    let b = fixture.order(
        "downstream.toml",
        r#"
id = "downstream"
title = "Build on dirty"
brief = "Write src/two.rs."
scope = ["src/two.rs"]
verify_profile = "fast"
after = ["dirty"]
"#,
    );

    let report = fixture.run_report(&[&a, &b], 1);
    let orders = report["orders"].as_array().unwrap();
    let dirty = orders.iter().find(|o| o["id"] == "dirty").unwrap();
    let downstream = orders.iter().find(|o| o["id"] == "downstream").unwrap();

    assert_eq!(dirty["outcome"], "verified", "{report}");
    assert!(
        dirty["candidate_commit"].is_null(),
        "a dirty candidate must not be identified by HEAD: {dirty}"
    );
    assert!(
        dirty["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("uncommitted work at finish"),
        "{dirty}"
    );

    assert_eq!(downstream["outcome"], "skipped", "{downstream}");
    assert!(
        downstream["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("immutable candidate"),
        "{downstream}"
    );
    assert!(
        downstream["worktree"].is_null(),
        "never dispatched: {downstream}"
    );
}
