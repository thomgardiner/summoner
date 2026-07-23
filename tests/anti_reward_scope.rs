//! Anti-reward-hacking integration (git host).
#[path = "anti_reward_support/mod.rs"]
mod support;
use support::*;

#[test]
fn git_host_rejects_crate_scope() {
    let env = GitEnv::new();
    env.write_config(
        r#"
default_executor = "t"
[host]
kind = "git"
[executors.t]
argv = ["true", "{prompt}"]
"#,
    );
    env.write_order(
        "cheat.toml",
        r#"
id = "cheat"
title = "t"
brief = "b"
scope = ["crate:secret"]
"#,
    );
    let out = env.cmd(&["plan", "orders/cheat.toml"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() || stdout.contains("revise"),
        "expected plan to refuse crate: on git host\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "plan json\nstdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(v["verdict"], "revise");
    let problems = v["problems"].as_array().cloned().unwrap_or_default();
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap_or("").contains("crate:")),
        "problems={problems:?}"
    );
}

/// Empty verify profile name that is not defined → doctor must not invent green.
#[test]
fn git_host_undefined_verify_profile_is_a_problem() {
    let env = GitEnv::new();
    env.write_config(
        r#"
default_executor = "t"
[host]
kind = "git"
[executors.t]
argv = ["true", "{prompt}"]
[verification]
required = ["real"]
[verification.profiles.real]
commands = [{ argv = ["true"], allow_zero_tests = true }]
"#,
    );
    env.write_order(
        "cheat.toml",
        r#"
id = "cheat"
title = "t"
brief = "b"
scope = ["README.md"]
verify_profile = "does-not-exist"
"#,
    );
    let out = env.cmd(&["doctor", "orders/cheat.toml"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() || stdout.contains("does-not-exist"),
        "doctor must flag undefined verify profile\n{stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(
        stdout
            .find('{')
            .map(|i| &stdout[i..])
            .unwrap_or(stdout.as_ref()),
    )
    .expect("doctor json");
    assert_eq!(v["ok"], false);
    let problems = v["orders"]["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        problems
            .iter()
            .any(|p| p.as_str().unwrap_or("").contains("does-not-exist")),
        "problems={problems:?}"
    );
}
