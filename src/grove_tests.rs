use super::*;

#[test]
fn begin_outcomes_parse_both_arms() {
    let begun: BeginOutcome = serde_json::from_str(
        r#"{"outcome":"begun","task":{"id":"abc-1","agent":"smn-x","extra_future_field":1}}"#,
    )
    .unwrap();
    let BeginOutcome::Begun { task } = begun else {
        panic!("expected begun");
    };
    assert_eq!(task.id, "abc-1");

    let conflict: BeginOutcome = serde_json::from_str(
        r#"{"outcome":"conflict","requested":["src"],"conflicts":[{"agent":"other"}]}"#,
    )
    .unwrap();
    let BeginOutcome::Conflict { conflicts } = conflict else {
        panic!("expected conflict");
    };
    assert_eq!(conflicts.len(), 1);
}

#[test]
fn finish_success_and_refusals_are_distinguished_by_the_outcome_key() {
    let finished = parse_finish(
        serde_json::json!({
            "task": {"id": "t", "verification": "passed"},
            "source_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "verification": {"required": ["fast"], "passed": ["fast"], "missing": [],
                             "stale": [], "failed": [], "verified": true}
        }),
        None,
    )
    .unwrap();
    let FinishOutcome::Finished { verification } = finished else {
        panic!("expected finished");
    };
    assert!(verification.verified);

    let refused = parse_finish(
        serde_json::json!({
            "outcome": "refused", "reason": "evidence",
            "verification": {"required": ["fast", "ci"], "passed": [], "missing": ["fast", "ci"],
                             "stale": [], "failed": [], "verified": false}
        }),
        None,
    )
    .unwrap();
    let FinishOutcome::Refused {
        reason,
        verification,
        ..
    } = refused
    else {
        panic!("expected refusal");
    };
    assert_eq!(reason, "evidence");
    assert_eq!(verification.unwrap().missing, ["fast", "ci"]);

    let scope = parse_finish(
        serde_json::json!({
            "outcome": "refused", "reason": "scope", "outside_scope": ["README.md"]
        }),
        None,
    )
    .unwrap();
    let FinishOutcome::Refused { outside_scope, .. } = scope else {
        panic!("expected refusal");
    };
    assert_eq!(outside_scope, ["README.md"]);
}

#[test]
fn bound_finish_requires_the_exact_returned_source_digest() {
    let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let success = |source: Option<&str>| {
        let mut value = serde_json::json!({
            "verification": {"required": ["fast"], "passed": ["fast"], "missing": [],
                             "stale": [], "failed": [], "verified": true}
        });
        if let Some(source) = source {
            value["source_sha256"] = source.into();
        }
        parse_finish(value, Some(expected))
    };
    assert!(success(Some(expected)).is_ok());
    for source in [None, Some("wrong")] {
        assert!(
            success(source)
                .err()
                .unwrap()
                .to_string()
                .contains("expected candidate")
        );
    }
}

#[test]
fn exec_argv_wraps_the_executor_behind_the_supervisor() {
    let grove = GroveCli::new("grove".into());
    let argv = grove.exec_argv("t-1", 900, &["codex".into(), "exec".into()]);
    assert_eq!(
        argv,
        [
            "grove",
            "task",
            "exec",
            "--capability",
            "edit",
            "--task-id",
            "t-1",
            "--timeout-secs",
            "900",
            "--",
            "codex",
            "exec"
        ]
    );
}
