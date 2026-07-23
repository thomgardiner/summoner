//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;

#[test]
fn repo_without_required_profiles_completes_with_the_override_recorded() {
    require_grove();
    let fixture = Fixture::new(false);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order(
        "wave.toml",
        r#"
id = "wave"
title = "Add the wave function"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
"#,
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "completed", "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("no required verification profiles"))
    );
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "finished".into())]
    );
}

#[test]
fn conflicting_scope_reports_blocked_without_dispatching() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("exit 0", 60);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let held = fixture.grove(&["claim", "--agent", "blocker", "--task", "hold", "src"]);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "blocked", "{report}");
    assert!(entry["conflicts"].as_array().is_some_and(|c| !c.is_empty()));
    assert!(entry["task_id"].is_null(), "no task was begun");
}

#[test]
fn sleeping_executor_is_stalled_by_the_grove_deadline_and_abandoned() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("sleep 30", 2);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "stalled", "{report}");
    assert_eq!(entry["executor_exit"], 124);
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}

#[test]
fn failing_executor_skips_verification_and_is_abandoned() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("exit 3", 60);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "executor_failed", "{report}");
    assert_eq!(entry["executor_exit"], 3);
    assert!(entry.get("verify").is_none(), "verification skipped");
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}

#[test]
fn out_of_scope_write_is_a_scope_violation() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\necho outside > outside.txt\n\
         git add -A\ngit commit -qm 'overreach'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "scope_violation", "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("outside.txt")),
        "{report}"
    );
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}
