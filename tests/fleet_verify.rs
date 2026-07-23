//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn clean_rust_example_finishes_with_real_grove_verification() {
    require_grove();
    let fixture = Fixture::new(false);
    fixture.commit_all("rust workspace");
    let initialized = fixture.summoner(&["init", "--example"]);
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    fixture.executor(
        "mkdir -p docs\nprintf '# Summoner demo\\n\\nThis crate demonstrates a verified Rust fleet.\\n' > docs/summoner-demo.md\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.repo.join("orders/example.toml");
    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["summary"]["verified"], 1, "{report}");
    assert_eq!(report["orders"][0]["verify"][0]["profile"], "rust-check");
}

const ORDER_TOML: &str = r#"
id = "wave"
title = "Add the wave function"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
acceptance = ["src/lib.rs gains a function", "work is committed"]
verify_profile = "fast"
"#;

#[test]
fn happy_path_order_is_verified_released_and_salvaged_to_its_branch() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "verified", "{report}");
    assert_eq!(entry["executor_exit"], 0);
    assert_eq!(entry["commits"], 1);
    assert_eq!(entry["finish"]["verified"], true);
    assert_eq!(report["summary"]["verified"], 1);

    // The reviewed candidate is identified by an immutable commit captured
    // before release, not just by a branch name that release can advance.
    let candidate = entry["candidate_commit"]
        .as_str()
        .expect("candidate commit recorded before release");
    assert_eq!(
        candidate.len(),
        40,
        "expected a full object id: {candidate}"
    );
    assert_ne!(candidate, entry["base_commit"].as_str().unwrap());
    let reachable = Command::new("git")
        .args(["cat-file", "-e", &format!("{candidate}^{{commit}}")])
        .current_dir(&fixture.repo)
        .status()
        .unwrap();
    assert!(
        reachable.success(),
        "candidate commit {candidate} must survive worktree release"
    );

    // The worktree is gone; the work survives on the order's branch.
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(!Path::new(worktree).exists(), "worktree released");
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains("pub fn wave()"));

    // No claim leaked: the task is terminal.
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "finished".into())]
    );

    // The run left its mark on the cross-run scorecard, and the aggregation
    // command reads it back per repo and executor.
    let output = fixture.summoner(&["scorecard"]);
    assert_eq!(output.status.code(), Some(0));
    let board: Value = serde_json::from_slice(&output.stdout).unwrap();
    let repo_key = board
        .as_object()
        .unwrap()
        .keys()
        .find(|key| key.contains("repo"))
        .expect("repo entry")
        .clone();
    let stats = &board[&repo_key]["fake"];
    assert_eq!(stats["orders"], 1, "{board}");
    assert_eq!(stats["green"], 1, "{board}");
    assert_eq!(stats["outcomes"]["verified"], 1, "{board}");
}

#[test]
fn clean_executor_exit_without_changes_is_not_verified() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("true", 60);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "unverified", "{report}");
    assert_eq!(entry["executor_exit"], 0);
    assert_eq!(entry["commits"], 0);
    assert_eq!(entry["diff"]["files_changed"], 0);
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("no changes")),
        "{report}"
    );
    assert!(entry.get("verify").is_none(), "{report}");
}

#[test]
fn executor_declaring_unmet_acceptance_is_not_verified() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor_raw(
        "echo 'pub fn wave() {}' >> src/lib.rs\n\
         git add -A\ngit commit -qm 'partial executor work'\n\
         echo '{\"summoner_status\":\"incomplete\",\"unmet\":[\"wire regression missing\"]}'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "unverified", "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("wire regression missing")),
        "{report}"
    );
    assert!(entry.get("verify").is_none(), "{report}");
}

#[test]
fn verifier_created_ignored_artifacts_do_not_block_release() {
    require_grove();
    let fixture = Fixture::new(true);
    std::fs::write(fixture.repo.join(".gitignore"), "verify.out\n").unwrap();
    std::fs::write(
        fixture.repo.join(".grove.toml"),
        GROVE_TOML.replace("[\"true\"]", "[\"sh\", \"-c\", \"touch verify.out\"]"),
    )
    .unwrap();
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "verified", "{report}");
    assert!(!Path::new(entry["worktree"].as_str().unwrap()).exists());
}

#[test]
fn executor_created_ignored_artifacts_remain_protected() {
    require_grove();
    let fixture = Fixture::new(true);
    std::fs::write(fixture.repo.join(".gitignore"), "executor.out\n").unwrap();
    fixture.executor(
        "echo private > executor.out\n\
         echo 'pub fn wave() {}' >> src/lib.rs\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "error", "{report}");
    assert!(
        entry["release_error"]
            .as_str()
            .is_some_and(|error| error.contains("ignored path")),
        "{report}"
    );
    let worktree = Path::new(entry["worktree"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(worktree.join("executor.out")).unwrap(),
        "private\n"
    );
}

#[test]
fn internal_error_after_commit_preserves_report_evidence() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let wrapper = fixture.base.path().join("failing-grove.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = verify ]; then echo 'forced verify failure' >&2; exit 2; fi\n\
             exec '{}' \"$@\"\n",
            grove_bin().replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let order = fixture.order("wave.toml", ORDER_TOML);

    let output = fixture.summoner_with_env(
        &["run", order.to_str().unwrap()],
        &[("SUMMONER_GROVE_BIN", wrapper.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "error", "{report}");
    assert_eq!(entry["commits"], 1, "{report}");
    assert_eq!(entry["diff"]["files_changed"], 1, "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("forced verify failure")),
        "{report}"
    );
}

#[test]
fn two_independent_orders_run_in_one_fleet_and_both_verify() {
    require_grove();
    let fixture = Fixture::new(true);
    // One script serves both orders: the worktree branch names the order, and
    // each order writes only its own declared file.
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) echo 'pub fn one() {}' > src/one.rs ;;\n\
           *smn-two) echo 'pub fn two() {}' > src/two.rs ;;\n\
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
title = "Touch file two"
brief = "Write src/two.rs and commit."
scope = ["src/two.rs"]
verify_profile = "fast"
"#,
    );

    let report = fixture.run_report(&[&a, &b], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    let states = fixture.task_states();
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|(_, status)| status == "finished"));
}
