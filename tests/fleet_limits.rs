//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;

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


