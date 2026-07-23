//! Anti-reward-hacking integration: git host refuses cheat scopes, does not
//! invent verified without profiles, and caps protected-config hacks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

/// Orders that use crate: on an explicit git host fail validation.
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

/// Passing required profile on git host → verified (not a vacuous green).
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

// --- fixture ---------------------------------------------------------------

struct GitEnv {
    root: TempDir,
    repo: PathBuf,
    config_home: PathBuf,
    cache: PathBuf,
    bin: PathBuf,
}

impl GitEnv {
    fn new() -> Self {
        let root = TempDir::new().expect("temp");
        let repo = root.path().join("repo");
        let config_home = root.path().join("cfg");
        let cache = root.path().join("cache");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.join("src/lib.txt"), "lib\n").unwrap();
        run(&repo, &["git", "init", "-q"]);
        run(&repo, &["git", "config", "user.email", "ar@test"]);
        run(&repo, &["git", "config", "user.name", "ar"]);
        run(&repo, &["git", "add", "-A"]);
        run(&repo, &["git", "commit", "-qm", "init"]);
        Self {
            root,
            repo,
            config_home,
            cache,
            bin,
        }
    }

    fn write_config(&self, body: &str) {
        let dir = self.config_home.join("summoner");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    fn write_order(&self, name: &str, body: &str) {
        std::fs::create_dir_all(self.repo.join("orders")).unwrap();
        std::fs::write(self.repo.join("orders").join(name), body).unwrap();
    }

    fn write_worker(&self, body: &str) -> PathBuf {
        let path = self.bin.join("smn-worker");
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn path_without_grove(&self) -> OsString {
        let mut parts = vec![self.bin.clone()];
        for system in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            let p = PathBuf::from(system);
            if p.is_dir() {
                parts.push(p);
            }
        }
        std::env::join_paths(parts).expect("join path")
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(SUMMONER)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", self.path_without_grove())
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("HOME", self.root.path())
            .env_remove("SUMMONER_GROVE_BIN")
            .output()
            .expect("summoner")
    }

    fn run_report(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cmd(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(
            out.status.code(),
            Some(2),
            "infra failure (exit {:?})\nstdout={stdout}\nstderr={stderr}",
            out.status.code()
        );
        let json = stdout
            .find('{')
            .map(|i| &stdout[i..])
            .unwrap_or(stdout.as_ref());
        serde_json::from_str(json).unwrap_or_else(|e| {
            panic!("report json: {e}\nstdout={stdout}\nstderr={stderr}");
        })
    }

    fn git(&self, args: &[&str]) {
        let mut v = Vec::with_capacity(args.len() + 1);
        v.push("git");
        v.extend_from_slice(args);
        run(&self.repo, &v);
    }
}

fn run(dir: &Path, argv: &[&str]) {
    assert!(
        Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "{argv:?} failed"
    );
}
