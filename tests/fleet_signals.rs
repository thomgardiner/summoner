//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn sigterm_tears_down_the_fleet_and_still_emits_a_partial_report() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("sleep 30", 60);
    let order = fixture.order(
        "slow.toml",
        "id = \"slow\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/slow.rs\"]\nverify_profile = \"fast\"\n",
    );

    let stdout_path = fixture.base.path().join("stream.ndjson");
    let mut summoner = Command::new(SUMMONER)
        .args(["run", "--stream", order.to_str().unwrap()])
        .current_dir(&fixture.repo)
        .env("SUMMONER_GROVE_BIN", grove_bin())
        .env("GROVE_CACHE_ROOT", fixture.base.path().join("cache"))
        .env("XDG_CACHE_HOME", fixture.base.path().join("xdg"))
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Interrupt only once the executor is actually running. A cold worktree
    // acquire is ~10s and stretches further under a loaded CI runner, so the
    // deadline covers that with margin rather than flaking on dispatch timing.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let streamed = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        if streamed.contains("order_dispatched") {
            break;
        }
        assert!(Instant::now() < deadline, "order never dispatched");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &summoner.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let exit = summoner.wait().unwrap();
    assert_eq!(exit.code(), Some(1), "interrupted run needs review");

    // The stream still ends with a complete report classifying the interrupt.
    let streamed = std::fs::read_to_string(&stdout_path).unwrap();
    let last: Value = serde_json::from_str(streamed.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "report", "{streamed}");
    let entry = &last["report"]["orders"][0];
    assert_eq!(entry["outcome"], "interrupted", "{streamed}");

    // No leaked claim, and the worktree came back.
    assert_eq!(
        fixture.task_states(),
        [("smn-slow".into(), "abandoned".into())]
    );
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(!Path::new(worktree).exists(), "worktree released");
}

#[test]
fn streamed_run_emits_ndjson_events_and_a_final_report_line() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let output = fixture.summoner(&["run", "--stream", order.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Every stdout line is one JSON object: the whole point of the stream.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    let names: Vec<&str> = lines
        .iter()
        .map(|line| line["event"].as_str().unwrap())
        .collect();
    assert_eq!(names.first(), Some(&"run_started"), "{names:?}");
    for expected in [
        "order_started",
        "order_dispatched",
        "order_exec_done",
        "order_verify",
        "order_checkpoint",
        "order_finished",
        "run_finished",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    assert_eq!(names.last(), Some(&"report"), "{names:?}");
    let report = &lines.last().unwrap()["report"];
    assert_eq!(report["orders"][0]["outcome"], "verified");

    // order_dispatched carries what a live consumer needs to follow along.
    let dispatched = lines
        .iter()
        .find(|line| line["event"] == "order_dispatched")
        .unwrap();
    assert!(dispatched["task_id"].is_string());
    assert!(dispatched["stdout_log"].is_string());

    // The sidecar log always exists, stream or not, and ends with run_finished.
    let run_dir = lines[0]["run_dir"].as_str().unwrap();
    let sidecar = std::fs::read_to_string(Path::new(run_dir).join("events.jsonl")).unwrap();
    let sidecar_names: Vec<String> = sidecar
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["event"].to_string())
        .collect();
    assert_eq!(sidecar_names.last().unwrap(), "\"run_finished\"");
    assert!(!sidecar.contains("\"event\":\"report\""));
}

#[test]
fn branch_switching_executor_downgrades_success_to_error() {
    require_grove();
    let fixture = Fixture::new(true);
    // In-scope work, receipts green — but the checkout leaves its leased
    // branch, so grove refuses the release. That worktree is leaked, and the
    // run must not call it a success.
    fixture.executor(
        "git checkout -qb rogue\necho 'pub fn r() {}' > src/rogue.rs\n\
         git add -A\ngit commit -qm rogue",
        60,
    );
    let order = fixture.order(
        "rogue.toml",
        r#"
id = "rogue"
title = "Wanders off its branch"
brief = "Commit in scope but on a different branch."
scope = ["src"]
verify_profile = "fast"
"#,
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "error", "{report}");
    assert!(
        entry["release_error"]
            .as_str()
            .is_some_and(|e| e.contains("manual recovery")),
        "{report}"
    );
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(
        Path::new(worktree).exists(),
        "leaked worktree still on disk"
    );
}
