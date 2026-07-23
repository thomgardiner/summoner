//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn approving_reviewer_upgrades_verified_to_approved_and_exit_zero() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    fixture.reviewer("echo '{\"verdict\":\"approve\",\"findings\":[]}'");
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "approved", "{report}");
    assert_eq!(entry["review"]["reviewer"], "judge", "{report}");
    assert_eq!(entry["review"]["verdict"], "approve", "{report}");
    let snapshot = entry["review"]["candidate_snapshot_sha256"]
        .as_str()
        .expect("report carries the reviewed snapshot digest");
    assert_eq!(snapshot.len(), 64, "{report}");
    assert!(entry["review"].get("candidate_source_sha256").is_none());
    assert!(entry["review"].get("candidate_tree").is_none());
    assert_eq!(entry["finish"]["verified"], true, "{report}");
    assert_eq!(report["summary"]["approved"], 1, "{report}");
    // The review prompt is on disk next to the executor's: independent record.
    let prompt = std::fs::read_to_string(
        PathBuf::from(entry["review"]["stdout_log"].as_str().unwrap())
            .parent()
            .unwrap()
            .join("review-prompt.md"),
    )
    .unwrap();
    assert!(
        prompt.contains("# Review charter"),
        "review charter present"
    );
    assert!(
        !prompt.contains("# Worker charter"),
        "implementer charter must not leak into the review"
    );
    // The gate is observable live: review_started names the logs to tail
    // before any verdict exists, and the verdict event follows it.
    let events_path = fixture
        .base
        .path()
        .join("xdg/summoner/runs")
        .join(report["run_id"].as_str().unwrap())
        .join("events.jsonl");
    let events = std::fs::read_to_string(&events_path).unwrap();
    let started_at = events.find("\"review_started\"").expect("review_started");
    let verdict_at = events.find("\"order_review\"").expect("order_review");
    assert!(started_at < verdict_at, "{events}");
    assert!(events.contains("review-stdout.log"), "{events}");
    let review_events = events
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            matches!(
                event["event"].as_str(),
                Some("review_started" | "order_review")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(review_events.len(), 2, "{events}");
    for event in review_events {
        assert_eq!(event["candidate_snapshot_sha256"], snapshot, "{event}");
        assert_eq!(event["diff_sha256"], entry["review"]["diff_sha256"]);
        assert!(event.get("source_sha256").is_none(), "{event}");
        assert!(event.get("candidate_tree").is_none(), "{event}");
    }
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "finished".into())]
    );
}

#[test]
fn orchestrator_profile_switches_the_reviewer_on_by_harness_marker() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    // The judge exists but is wired only through the claude profile: which
    // orchestrator invokes summoner decides whether the gate is on.
    let script = fixture.reviewer_script("echo '{\"verdict\":\"approve\",\"findings\":[]}'");
    let existing = std::fs::read_to_string(fixture.repo.join(".summoner.toml")).unwrap();
    std::fs::write(
        fixture.repo.join(".summoner.toml"),
        format!(
            "{existing}\n[executors.judge]\nargv = [\"{}\"]\nprompt = \"stdin\"\n\
             timeout_secs = 60\n\n[profiles.claude]\ndefault_reviewer = \"judge\"\n",
            script.display()
        ),
    )
    .unwrap();
    fixture.commit_all("profile fixture");
    let order = fixture.order("wave.toml", ORDER_TOML);

    // Bare invocation: no marker, no profile, no gate.
    let output = fixture.summoner(&["run", order.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["orders"][0]["outcome"], "verified", "{report}");

    // The same command from a Claude Code shell auto-selects [profiles.claude].
    let output =
        fixture.summoner_with_env(&["run", order.to_str().unwrap()], &[("CLAUDECODE", "1")]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["orders"][0]["outcome"], "approved", "{report}");
    assert_eq!(
        report["orders"][0]["review"]["reviewer"], "judge",
        "{report}"
    );
}

#[test]
fn rejected_work_is_revised_with_findings_in_a_resumed_session() {
    require_grove();
    let fixture = Fixture::new(true);
    // Attempt 1: prints a session banner, commits work the judge will reject.
    fixture.executor(
        "echo 'session id: sess-42'\n\
         echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w",
        60,
    );
    // The revision resumes the session: a different script that receives
    // {session_id}, records it, and fixes the work.
    let resume = fixture.base.path().join("resume-executor.sh");
    std::fs::write(
        &resume,
        format!(
            "#!/bin/sh\necho \"$1\" > {}\ncat > {}\n\
             echo 'pub fn fixed() {{}}' >> src/lib.rs\ngit add -A\ngit commit -qm fix\n\
             echo '{{\"summoner_status\":\"complete\",\"unmet\":[]}}'\n",
            fixture.base.path().join("resumed-with.txt").display(),
            fixture.base.path().join("revision-prompt.txt").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&resume, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.append_config(&format!(
        "session_marker = \"session id:\"\nresume_argv = [\"{}\", \"{{session_id}}\"]",
        resume.display()
    ));
    // The judge rejects the first attempt and approves the second.
    let flag = fixture.base.path().join("judged-once");
    fixture.reviewer(&format!(
        "if [ -f {flag} ]; then echo '{{\"verdict\":\"approve\",\"findings\":[]}}'; \
         else touch {flag}; \
         echo '{{\"verdict\":\"reject\",\"findings\":[{{\"severity\":\"blocker\",\"file\":\"src/lib.rs\",\"line\":1,\"summary\":\"wave is wrong\"}}]}}'; fi",
        flag = flag.display()
    ));
    let order = fixture.order("wave.toml", ORDER_TOML);

    let output = fixture.summoner_with_env(
        &["run", order.to_str().unwrap()],
        &[("SUMMONER_REVISE", "1")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "approved", "{report}");
    assert_eq!(entry["attempts"], 2, "{report}");
    assert_eq!(entry["session_id"], "sess-42", "{report}");
    assert_eq!(entry["review"]["verdict"], "approve", "{report}");

    // The resume template really received the captured session id...
    let resumed = std::fs::read_to_string(fixture.base.path().join("resumed-with.txt")).unwrap();
    assert_eq!(resumed.trim(), "sess-42");
    // ...and the revision prompt carried the findings, not the full charter
    // (the session already has it).
    let prompt = std::fs::read_to_string(fixture.base.path().join("revision-prompt.txt")).unwrap();
    assert!(prompt.contains("wave is wrong"), "{prompt}");
    assert!(!prompt.contains("# Worker charter"), "{prompt}");

    // Both attempts' work landed on the branch.
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    let lib = String::from_utf8_lossy(&show.stdout).into_owned();
    assert!(lib.contains("pub fn wave()"), "{lib}");
    assert!(lib.contains("pub fn fixed()"), "{lib}");
}

#[test]
fn rejecting_reviewer_downgrades_verified_work_and_carries_findings() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    fixture.reviewer(
        "echo '{\"verdict\":\"reject\",\"findings\":[{\"severity\":\"blocker\",\"file\":\"src/lib.rs\",\"line\":1,\"summary\":\"hardcoded expected value\"}]}'",
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "rejected", "{report}");
    assert_eq!(
        entry["review"]["findings"][0]["summary"], "hardcoded expected value",
        "{report}"
    );
    // The work is still finished and salvaged: the orchestrator reviews the
    // findings against a real branch, not a lost worktree.
    assert_eq!(entry["finish"]["verified"], true, "{report}");
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains("pub fn wave()"));
}

#[test]
fn a_reviewer_that_mutates_its_capsule_voids_the_verdict_and_source_stays_exact() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    // The worst reviewer: plants an untracked file AND stages a malicious
    // edit to an in-scope file before approving its own tampering.
    fixture.reviewer(
        "chmod u+w . src src/lib.rs\n\
         echo sneaky > planted.txt\n\
         echo 'pub fn sneak() {}' >> src/lib.rs\ngit add src/lib.rs\n\
         echo '{\"verdict\":\"approve\",\"findings\":[]}'",
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "review_failed", "{report}");
    assert!(
        entry["review"]["detail"]
            .as_str()
            .unwrap()
            .contains("did not authorize"),
        "{report}"
    );
    // Neither write reaches the branch; the executor's work does.
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:planted.txt"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(!show.status.success(), "planted file must not be salvaged");
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    let lib = String::from_utf8_lossy(&show.stdout).into_owned();
    assert!(lib.contains("pub fn wave()"), "{lib}");
    assert!(!lib.contains("sneak"), "staged edit must be undone: {lib}");
}

#[test]
fn finish_cas_rejects_a_candidate_mutated_and_reverted_during_finish() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    fixture.reviewer("echo '{\"verdict\":\"approve\",\"findings\":[]}'");
    let order = fixture.order("wave.toml", ORDER_TOML);
    let wrapper = fixture.base.path().join("grove-finish-race.sh");
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
case " $* " in
  *" --expected-source-sha256 "*)
    cp src/lib.rs "$MUTATE_SAVE"
    echo 'pub fn raced() {}' >> src/lib.rs
    "$REAL_GROVE" "$@" > "$WRAP_OUT" 2> "$WRAP_ERR"
    code=$?
    cp "$MUTATE_SAVE" src/lib.rs
    cat "$WRAP_OUT"
    cat "$WRAP_ERR" >&2
    exit "$code" ;;
  *) exec "$REAL_GROVE" "$@" ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = fixture
        .summoner_command(&["run", order.to_str().unwrap()], wrapper.to_str().unwrap())
        .env("REAL_GROVE", grove_bin())
        .env("MUTATE_SAVE", fixture.base.path().join("source.saved"))
        .env("WRAP_OUT", fixture.base.path().join("finish.stdout"))
        .env("WRAP_ERR", fixture.base.path().join("finish.stderr"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["orders"][0]["outcome"], "unverified", "{report}");
    assert!(
        report["orders"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("changed after review"),
        "{report}"
    );
    let saved = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    let saved = String::from_utf8_lossy(&saved.stdout);
    assert!(saved.contains("wave"), "{saved}");
    assert!(!saved.contains("raced"), "{saved}");
}

#[test]
fn modifying_verification_config_caps_the_outcome_at_unverified() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo '# weakened' >> .grove.toml\ngit add -A\ngit commit -qm 'hack the verifier'",
        60,
    );
    let order = fixture.order(
        "hack.toml",
        r#"
id = "hack"
title = "Try to weaken the gate"
brief = "Modify the verification config."
scope = ["src", ".grove.toml"]
verify_profile = "fast"
"#,
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "unverified", "{report}");
    assert!(
        entry["detail"].as_str().unwrap().contains("protected"),
        "{report}"
    );
    assert_eq!(
        entry["tripwires"][0], "protected file modified: .grove.toml",
        "{report}"
    );
    // Nothing was verified or finished on a compromised config.
    assert_eq!(
        fixture.task_states(),
        [("smn-hack".into(), "abandoned".into())]
    );
}
