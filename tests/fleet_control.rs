//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn run_token_budget_skips_the_rest_of_the_queue() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn f() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w\necho 'tokens used'\necho 500",
        60,
    );
    fixture.append_config("usage_marker = \"tokens used\"");
    let a = fixture.order(
        "a.toml",
        "id = \"a\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nverify_profile = \"fast\"\n",
    );
    let b = fixture.order(
        "b.toml",
        "id = \"b\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"docs\"]\nverify_profile = \"fast\"\n",
    );

    let output = fixture.summoner_with_env(
        &["run", a.to_str().unwrap(), b.to_str().unwrap()],
        &[
            ("SUMMONER_MAX_PARALLEL", "1"),
            ("SUMMONER_RUN_TOKEN_BUDGET", "100"),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["verified"], 1, "{report}");
    assert_eq!(report["summary"]["skipped"], 1, "{report}");
    let skipped = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["outcome"] == "skipped")
        .unwrap();
    assert!(
        skipped["detail"]
            .as_str()
            .unwrap()
            .contains("budget exhausted"),
        "{report}"
    );
}

#[test]
fn variants_race_the_same_scope_and_both_land_on_their_own_branches() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn from_fake() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'fake work'",
        60,
    );
    // A second backend racing the same order: same scope, different output.
    let script = fixture.base.path().join("fake2-executor.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'pub fn from_fake2() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'fake2 work'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.append_config(&format!(
        "\n[executors.fake2]\nargv = [\"{}\"]\nprompt = \"stdin\"\ntimeout_secs = 60",
        script.display()
    ));
    let order = fixture.order(
        "race.toml",
        r#"
id = "race"
title = "Race two executors over one order"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
verify_profile = "fast"
variants = ["fake", "fake2"]
"#,
    );

    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    for (entry, executor) in report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["fake", "fake2"])
    {
        assert_eq!(entry["id"], format!("race-{executor}"), "{report}");
        assert_eq!(entry["outcome"], "verified", "{report}");
        assert_eq!(entry["executor"], executor, "{report}");
        assert_eq!(entry["variant_of"], "race", "{report}");
        assert_eq!(
            entry["branch"],
            format!("grove/smn-race-{executor}"),
            "{report}"
        );
    }

    // Each attempt survives on its own branch; the orchestrator lands a winner.
    for (executor, marker) in [("fake", "from_fake()"), ("fake2", "from_fake2()")] {
        let show = Command::new("git")
            .args(["show", &format!("grove/smn-race-{executor}:src/lib.rs")])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&show.stdout).contains(marker),
            "branch for {executor} missing {marker}"
        );
    }

    let mut states = fixture.task_states();
    states.sort();
    assert_eq!(
        states,
        [
            ("smn-race-fake".into(), "finished".into()),
            ("smn-race-fake2".into(), "finished".into()),
        ]
    );
}

#[test]
fn plan_refutes_a_batch_then_passes_it_once_revised() {
    require_grove();
    let (base, repo) = two_crate_workspace();
    std::fs::write(
        repo.join(".summoner.toml"),
        "default_executor = \"fake\"\n\
         [executors.fake]\nargv = [\"true\"]\nprompt = \"stdin\"\ntimeout_secs = 60\n\
         [executors.fake2]\nargv = [\"true\"]\nprompt = \"stdin\"\ntimeout_secs = 60\n",
    )
    .unwrap();
    let orders = base.path().join("orders");
    std::fs::create_dir_all(&orders).unwrap();
    std::fs::write(
        orders.join("core.toml"),
        "id = \"core-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:core\"]\n",
    )
    .unwrap();
    std::fs::write(
        orders.join("app.toml"),
        "id = \"app-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:app\"]\n",
    )
    .unwrap();

    let plan = |expect_exit: i32| -> Value {
        let output = Command::new(SUMMONER)
            .args(["plan", orders.to_str().unwrap()])
            .current_dir(&repo)
            .env("SUMMONER_GROVE_BIN", grove_bin())
            .env("GROVE_CACHE_ROOT", base.path().join("cache"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expect_exit),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    // Package topology is useful context, but disjoint work orders may run in
    // parallel even when one package depends on the other.
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    assert!(report["missing_after"].is_null(), "{report}");
    assert_eq!(
        report["partition"]["couplings"][0]["kind"], "dependency",
        "{report}"
    );

    // Declare the suggested edge: clean, with a two-wave schedule.
    std::fs::write(
        orders.join("app.toml"),
        "id = \"app-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:app\"]\nafter = [\"core-work\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    assert_eq!(
        report["partition"]["waves"],
        serde_json::json!([["core-work"], ["app-work"]])
    );

    // A genuine claim conflict needs ordering.
    std::fs::write(
        orders.join("clash.toml"),
        "id = \"clash\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crates/core/src\"]\n",
    )
    .unwrap();
    let report = plan(1);
    assert_eq!(report["verdict"], "revise", "{report}");
    assert!(
        report["partition"]["conflicts"]
            .as_array()
            .is_some_and(|c| !c.is_empty()),
        "{report}"
    );

    // Once explicitly serialized, the overlap is dispatchable.
    std::fs::write(
        orders.join("clash.toml"),
        "id = \"clash\"\ntitle = \"t\"\nbrief = \"b\"\n\
         scope = [\"crates/core/src\"]\nafter = [\"core-work\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");

    // Variant siblings deliberately share scope; their claim group flows
    // through the partition, so an N-version race plans clean.
    std::fs::write(
        orders.join("race.toml"),
        "id = \"race\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"README.md\"]\n\
         variants = [\"fake\", \"fake2\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    // The ordered clash conflict remains listed; the point is the siblings'
    // deliberate overlap never registers as one.
    for conflict in report["partition"]["conflicts"].as_array().unwrap() {
        for side in ["a", "b"] {
            assert!(
                !conflict[side]
                    .as_str()
                    .is_some_and(|id| id.starts_with("race-")),
                "{report}"
            );
        }
    }
}

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
fn fail_fast_skips_the_remaining_queue_after_the_threshold() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor("exit 3", 60);
    // One worker, so exactly one failure lands before the breaker decides
    // about the rest of the queue.
    let config = std::fs::read_to_string(fixture.repo.join(".summoner.toml"))
        .unwrap()
        .replace("max_parallel = 2", "max_parallel = 1");
    std::fs::write(
        fixture.repo.join(".summoner.toml"),
        format!("fail_fast = 1\n{config}"),
    )
    .unwrap();
    fixture.commit_all("fail fast config");

    let a = fixture.order(
        "a.toml",
        "id = \"one\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/one.rs\"]\n",
    );
    let b = fixture.order(
        "b.toml",
        "id = \"two\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/two.rs\"]\n",
    );
    let c = fixture.order(
        "c.toml",
        "id = \"zzz\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/three.rs\"]\n",
    );

    let report = fixture.run_report(&[&a, &b, &c], 1);
    assert_eq!(report["summary"]["executor_failed"], 1, "{report}");
    assert_eq!(report["summary"]["skipped"], 2, "{report}");
    let skipped = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["outcome"] == "skipped")
        .unwrap();
    assert!(
        skipped["detail"]
            .as_str()
            .is_some_and(|d| d.contains("fail_fast")),
        "{report}"
    );
}

#[test]
fn usage_marker_records_tokens_per_order_and_per_run() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm work\n\
         echo 'tokens used'\necho '1,234'",
        60,
    );
    fixture.append_config("usage_marker = \"tokens used\"");
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["orders"][0]["usage_tokens"], 1234, "{report}");
    assert_eq!(report["usage_tokens"], 1234, "{report}");
}

/// The prompt-cache split is read from Claude's `--output-format json` result
/// envelope, so a run can show whether the fleet is reading context warm from
/// cache or paying to write it cold. The fake executor emits the real envelope
/// shape, including the per-turn `usage.iterations` copy whose smaller numbers
/// must NOT be what lands in the report — only the cumulative top-level count.

#[test]
fn cache_split_is_read_from_the_claude_json_envelope() {
    require_grove();
    let fixture = Fixture::new(true);
    // Captured from a real `claude --print --output-format json` run: cumulative
    // read 66448 / write 8494, shadowed by a nested per-turn 24807 / 614. The
    // whole envelope is one echoed line so stdout is exactly the JSON.
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm work\n\
         echo '[{\"type\":\"assistant\",\"message\":{\"usage\":{\"cache_read_input_tokens\":40870,\"cache_creation_input_tokens\":7880}}},\
         {\"type\":\"result\",\"subtype\":\"success\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":8494,\"cache_read_input_tokens\":66448,\"output_tokens\":4,\"iterations\":[{\"input_tokens\":2,\"output_tokens\":4,\"cache_read_input_tokens\":24807,\"cache_creation_input_tokens\":614}]}}]'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["orders"][0]["cache_read_tokens"], 66448, "{report}");
    assert_eq!(report["orders"][0]["cache_write_tokens"], 8494, "{report}");
    // The run-level rollup sums the split across orders.
    assert_eq!(report["cache_read_tokens"], 66448, "{report}");
    assert_eq!(report["cache_write_tokens"], 8494, "{report}");
}

/// A configured usage_marker that never matches the executor's output must
/// surface as a tripwire on the report — including the successful verified
/// path, where a prior version had the diff scan overwrite the warning. A
/// silent hole in budget accounting is exactly what this warning exists to
/// prevent.

#[test]
fn an_unmatched_usage_marker_warns_on_the_verified_path() {
    require_grove();
    let fixture = Fixture::new(true);
    // The executor commits real work (so the order verifies) but never prints
    // the configured marker, so token usage cannot be scraped.
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm work",
        60,
    );
    fixture.append_config("usage_marker = \"tokens used\"");
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "verified", "{report}");
    assert!(
        entry["usage_tokens"].is_null(),
        "nothing was scraped: {entry}"
    );
    let tripwires = entry["tripwires"].as_array().expect("tripwires present");
    assert!(
        tripwires
            .iter()
            .any(|t| t.as_str().unwrap_or_default().contains("usage_marker")
                && t.as_str().unwrap_or_default().contains("never matched")),
        "the unmatched-marker warning must survive to the report: {entry}"
    );
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
