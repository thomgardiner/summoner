//! Anti-reward and fault-injection coverage for the host capability bar,
//! trusted policy pins, and judge-input protection.

#[path = "anti_reward_support/mod.rs"]
mod support;
use support::*;

#[test]
fn trusted_policy_required_host_refuses_git_when_grove_is_demanded() {
    let env = GitEnv::new();
    env.write_config(
        r#"
default_executor = "t"
[host]
kind = "git"
[executors.t]
argv = ["true", "{prompt}"]
identity = "vendor:a"
[trusted_policy]
required_host = "grove"
"#,
    );
    env.write_order(
        "o.toml",
        r#"
id = "o"
title = "t"
brief = "b"
scope = ["README.md"]
"#,
    );
    let out = env.cmd(&["plan", "orders/o.toml"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // plan may revise or doctor-fail; either path must name the host pin.
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("requires host") || combined.contains("grove") || !out.status.success(),
        "expected host pin enforcement\n{combined}"
    );
}

#[test]
fn tripwire_flags_crucible_and_lockfile_as_protected() {
    // Unit-level via scan helper is covered in tripwires tests; this is the
    // git-host integration: touching .crucible caps at unverified.
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
mkdir -p .crucible
printf 'weak\n' > .crucible/adapter.json
git add .crucible/adapter.json
git commit -qm "weaken judge"
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
        "cheat.toml",
        r#"
id = "cheat"
title = "t"
brief = "b"
scope = [".crucible/adapter.json"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/cheat.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    // In-scope .crucible edit is allowed by scope but tripwire must still flag it.
    assert!(
        outcome == "unverified"
            || report["orders"][0]["detail"]
                .as_str()
                .unwrap_or("")
                .contains("protected"),
        "judge input touch must not verify: {report}"
    );
}

#[test]
fn distinct_identity_refuses_same_model_aliases() {
    let env = GitEnv::new();
    env.write_config(
        r#"
default_executor = "a"
[host]
kind = "git"
[executors.a]
argv = ["true", "{prompt}"]
identity = "openai:shared"
[executors.b]
argv = ["true", "{prompt}"]
identity = "openai:shared"
[trusted_policy]
require_reviewer = true
distinct_reviewer_name = true
distinct_reviewer_identity = true
allowed_executors = ["a"]
allowed_reviewers = ["b"]
allowed_profiles = ["fast"]
[verification]
required = ["fast"]
[verification.profiles.fast]
commands = [{ argv = ["true"], allow_zero_tests = true }]
"#,
    );
    env.write_order(
        "o.toml",
        r#"
id = "o"
title = "t"
brief = "b"
scope = ["README.md"]
executor = "a"
reviewer = "b"
verify_profile = "fast"
"#,
    );
    let out = env.cmd(&["plan", "orders/o.toml"]);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("identity") || combined.contains("revise") || !out.status.success(),
        "same model identity must not plan clean\n{combined}"
    );
}
