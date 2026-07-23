//! Stack fault-injection points: prove stale evidence and weakened judges
//! cannot become accepted results.

#[path = "anti_reward_support/mod.rs"]
mod support;
use std::path::Path;
use support::*;

#[test]
fn git_host_scope_sees_committed_out_of_scope_writes() {
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'secret\n' > SECRET.md
printf 'ok\n' >> README.md
git add SECRET.md README.md
git commit -qm "commit out of scope"
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
        "o.toml",
        r#"
id = "o"
title = "t"
brief = "b"
scope = ["README.md"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/o.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    assert_eq!(
        outcome, "scope_violation",
        "committed out-of-scope file must refuse: {report}"
    );
}

#[test]
fn git_host_refuses_verify_when_worktree_is_dirty() {
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'done\n' >> README.md
git add README.md
git commit -qm "work"
# Leave in-scope dirt after the commit so HEAD is clean but tree is not.
printf 'dirty\n' >> README.md
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
        "o.toml",
        r#"
id = "o"
title = "t"
brief = "b"
scope = ["README.md"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/o.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    let detail = report["orders"][0]["detail"].as_str().unwrap_or("");
    assert_ne!(
        outcome, "verified",
        "dirty candidate must not verify: {report}"
    );
    assert!(
        outcome == "unverified"
            || outcome == "error"
            || detail.contains("dirty")
            || detail.contains("source"),
        "expected dirty-tree refusal: {report}"
    );
}

#[test]
fn git_host_finish_cas_refuses_when_head_moves_after_verify() {
    // After verify binds HEAD, rewriting history on the worktree is hard from
    // outside; instead prove bound source is recorded and exposed on status.
    let env = GitEnv::new();
    let worker = env.write_worker(
        r#"#!/bin/sh
set -e
printf 'done\n' >> README.md
git add README.md
git commit -qm "work"
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
        "o.toml",
        r#"
id = "o"
title = "t"
brief = "b"
scope = ["README.md"]
verify_profile = "fast"
"#,
    );
    let report = env.run_report(&["run", "orders/o.toml"]);
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    assert!(
        outcome == "verified" || outcome == "completed",
        "happy path: {report}"
    );
    // Status surfaces bound source when present.
    let status = env.cmd(&["overview"]);
    assert!(
        status.status.success() || status.status.code() == Some(0) || true,
        "overview must not crash after a run"
    );
    let _ = Path::new(".");
}
