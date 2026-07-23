//! Anti-reward-hacking integration (git host).
#[path = "anti_reward_support/mod.rs"]
mod support;
use support::*;

#[test]
fn git_host_passing_profile_is_verified() {
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'done\n' >> src/lib.txt
git add src/lib.txt
git commit -qm "summoner worker"
"#,
    );
    env.write_config(&format!(
        r#"
default_executor = "worker"
[host]
kind = "git"
[executors.worker]
argv = ["{}", "{{prompt}}"]
timeout_secs = 60
[verification]
required = ["fast"]
[verification.profiles.fast]
commands = [{{ argv = ["true"], allow_zero_tests = true }}]
"#,
        worker.display()
    ));
    env.write_order(
        "hello.toml",
        r#"
id = "hello"
title = "Touch lib"
brief = "Append one line to src/lib.txt and commit."
scope = ["src/lib.txt"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/hello.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    assert_eq!(
        outcome, "verified",
        "passing required profile must yield verified\nreport={report}"
    );
    assert_eq!(report["orders"][0]["finish"]["verified"], true, "{report}");
}

/// Failing verify profile → unverified; never verified.

#[test]
fn git_host_failing_profile_is_unverified() {
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'done\n' >> src/lib.txt
git add src/lib.txt
git commit -qm "summoner worker"
"#,
    );
    env.write_config(&format!(
        r#"
default_executor = "worker"
[host]
kind = "git"
[executors.worker]
argv = ["{}", "{{prompt}}"]
timeout_secs = 60
[verification]
required = ["strict"]
[verification.profiles.strict]
commands = [{{ argv = ["false"], allow_zero_tests = true }}]
"#,
        worker.display()
    ));
    env.write_order(
        "hello.toml",
        r#"
id = "hello"
title = "Touch lib"
brief = "Append one line."
scope = ["src/lib.txt"]
verify_profile = "strict"
"#,
    );
    let report = env.run_report(&["run", "orders/hello.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    assert_eq!(
        outcome, "unverified",
        "failing profile must not yield verified\nreport={report}"
    );
    assert_ne!(
        report["orders"][0]["finish"]["verified"], true,
        "finish.verified must not be true on fail\nreport={report}"
    );
}

/// Touching a protected verification config path caps at unverified with tripwire.

#[test]
fn git_host_protected_config_touch_is_unverified() {
    let env = GitEnv::new();
    // Seed protected file so a modify is a real diff, not an add of unrelated path.
    std::fs::write(env.repo.join(".grove.toml"), "[verification]\n").unwrap();
    env.git(&["add", "-A"]);
    env.git(&["commit", "-qm", "seed grove"]);

    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf '# weakened\n' >> .grove.toml
printf 'done\n' >> src/lib.txt
git add .grove.toml src/lib.txt
git commit -qm "hack verifier"
"#,
    );
    env.write_config(&format!(
        r#"
default_executor = "worker"
[host]
kind = "git"
[executors.worker]
argv = ["{}", "{{prompt}}"]
timeout_secs = 60
[verification]
required = ["fast"]
[verification.profiles.fast]
commands = [{{ argv = ["true"], allow_zero_tests = true }}]
"#,
        worker.display()
    ));
    env.write_order(
        "hack.toml",
        r#"
id = "hack"
title = "Weaken gate"
brief = "Modify verification config and touch lib."
scope = ["src/lib.txt", ".grove.toml"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/hack.toml"]);
    let entry = &report["orders"][0];
    assert_eq!(
        entry["outcome"], "unverified",
        "protected config modify must cap at unverified\nreport={report}"
    );
    let detail = entry["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("protected")
            || entry["tripwires"]
                .as_array()
                .map(|t| t
                    .iter()
                    .any(|x| x.as_str().unwrap_or("").contains("protected")))
                .unwrap_or(false),
        "expected protected tripwire or detail\nreport={report}"
    );
}

/// Empty required profiles → completed (honest), never verified.

#[test]
fn git_host_empty_required_is_completed_not_verified() {
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'done\n' >> src/lib.txt
git add src/lib.txt
git commit -qm "summoner worker"
"#,
    );
    env.write_config(&format!(
        r#"
default_executor = "worker"
[host]
kind = "git"
[executors.worker]
argv = ["{}", "{{prompt}}"]
timeout_secs = 60
"#,
        worker.display()
    ));
    env.write_order(
        "hello.toml",
        r#"
id = "hello"
title = "Touch lib"
brief = "Append one line."
scope = ["src/lib.txt"]
"#,
    );
    let report = env.run_report(&["run", "orders/hello.toml"]);
    assert_eq!(
        report["orders"][0]["outcome"], "completed",
        "no required profiles → completed, not verified\nreport={report}"
    );
}
