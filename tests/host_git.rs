//! Independence: fleet under the git host with no Grove binary on PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

#[test]
fn git_host_plan_and_run_without_grove_on_path() {
    let root = TempDir::new().expect("temp");
    let repo = root.path().join("repo");
    let cache = root.path().join("cache");
    let config_home = root.path().join("config");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&config_home).unwrap();

    init_repo(&repo);
    let worker = write_worker(&bin);
    write_summoner_config(&config_home, &worker);
    write_order(&repo);

    // PATH with only our worker + system essentials, but no grove.
    let path = stripped_path_without_grove(&bin);

    let plan = Command::new(SUMMONER)
        .args(["plan", "orders/hello.toml"])
        .current_dir(&repo)
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", root.path())
        .env_remove("SUMMONER_GROVE_BIN")
        .output()
        .expect("plan");
    assert!(
        plan.status.success(),
        "plan failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan_json: serde_json::Value = serde_json::from_slice(&plan.stdout).expect("plan json");
    assert_eq!(plan_json["verdict"], "clean", "{plan_json}");

    let run = Command::new(SUMMONER)
        .args(["run", "orders/hello.toml"])
        .current_dir(&repo)
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", root.path())
        .env_remove("SUMMONER_GROVE_BIN")
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    // Exit 1 is a domain outcome (needs review / not all verified). Exit 2 is infra.
    assert_ne!(
        run.status.code(),
        Some(2),
        "run infra failure (exit {:?})\nstdout={stdout}\nstderr={stderr}",
        run.status.code(),
    );
    // Pretty report may be the whole stdout; tolerate a leading notice line.
    let json = stdout
        .find('{')
        .map(|i| &stdout[i..])
        .unwrap_or(stdout.as_ref());
    let report: serde_json::Value = serde_json::from_str(json).unwrap_or_else(|e| {
        panic!("report json: {e}\nstdout={stdout}\nstderr={stderr}");
    });
    let outcome = report["orders"][0]["outcome"].as_str().unwrap_or("");
    // No [verification] profiles → honest completed, not fake verified.
    assert_eq!(
        outcome, "completed",
        "git host without verify profiles must complete, not claim verified\nreport={report}\nstderr={stderr}"
    );
}

fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("src/lib.txt"), "lib\n").unwrap();
    run(repo, &["git", "init", "-q"]);
    run(repo, &["git", "config", "user.email", "host-git@test"]);
    run(repo, &["git", "config", "user.name", "host-git"]);
    run(repo, &["git", "add", "-A"]);
    run(repo, &["git", "commit", "-qm", "init"]);
}

fn write_worker(bin: &Path) -> PathBuf {
    let path = bin.join("smn-worker");
    // Worktree is cwd. Touch only scoped path and commit.
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -e
printf 'done\n' >> src/lib.txt
git add src/lib.txt
git commit -qm "summoner worker"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

fn write_summoner_config(config_home: &Path, worker: &Path) {
    let dir = config_home.join("summoner");
    std::fs::create_dir_all(&dir).unwrap();
    let worker = worker.display();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            r#"
default_executor = "worker"

[host]
kind = "git"

[executors.worker]
argv = ["{worker}", "{{prompt}}"]
timeout_secs = 60
"#
        ),
    )
    .unwrap();
}

fn write_order(repo: &Path) {
    std::fs::create_dir_all(repo.join("orders")).unwrap();
    std::fs::write(
        repo.join("orders/hello.toml"),
        r#"
id = "hello"
title = "Touch lib"
brief = "Append one line to src/lib.txt and commit."
scope = ["src/lib.txt"]
acceptance = []
"#,
    )
    .unwrap();
}

fn stripped_path_without_grove(extra: &Path) -> OsString {
    // Minimal PATH: our worker first, then system bins that provide git/sh.
    // Deliberately omit cargo home / ~/.cargo/bin so `grove` is not found.
    let mut parts = vec![extra.to_path_buf()];
    for system in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = PathBuf::from(system);
        if p.is_dir() {
            parts.push(p);
        }
    }
    std::env::join_paths(parts).expect("join path")
}

use std::ffi::OsString;

fn run(dir: &Path, argv: &[&str]) {
    let status = Command::new(argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("run {argv:?}: {e}"));
    assert!(status.success(), "{argv:?} failed");
}

#[allow(dead_code)]
fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
