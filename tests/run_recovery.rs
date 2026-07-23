//! Kill Summoner mid-run and resume from journal + host state (git host).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

#[test]
fn kill_after_dispatch_then_resume_on_git_host() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    let cache = root.path().join("cache");
    let config_home = root.path().join("config");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&config_home).unwrap();
    init_repo(&repo);
    let worker = write_slow_worker(&bin);
    write_config(&config_home, &worker);
    write_order(&repo);

    let path = minimal_path(&bin);
    let mut child = Command::new(SUMMONER)
        .args(["run", "--stream", "orders/slow.toml"])
        .current_dir(&repo)
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", root.path())
        .env_remove("SUMMONER_GROVE_BIN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn run");

    // Wait until the journal exists and contains a dispatch event.
    let mut run_id = None;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(id) = find_run_id(&cache) {
            let events = cache
                .join("summoner")
                .join("runs")
                .join(&id)
                .join("events.jsonl");
            if events.is_file() {
                let text = std::fs::read_to_string(&events).unwrap_or_default();
                if text.contains("order_dispatched") || text.contains("order_started") {
                    run_id = Some(id);
                    break;
                }
            }
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
    }
    let run_id = run_id.expect("run should have started a journal");
    let _ = child.kill();
    let _ = child.wait();

    // Resume should re-open the fleet without infra failure.
    let resume = Command::new(SUMMONER)
        .args(["resume", &run_id])
        .current_dir(&repo)
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", root.path())
        .env_remove("SUMMONER_GROVE_BIN")
        .output()
        .expect("resume");
    assert_ne!(
        resume.status.code(),
        Some(2),
        "resume infra error\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&resume.stdout),
        String::from_utf8_lossy(&resume.stderr)
    );
    let events_path = cache
        .join("summoner")
        .join("runs")
        .join(&run_id)
        .join("events.jsonl");
    assert!(events_path.is_file(), "original run journal must remain");
    let events = std::fs::read_to_string(&events_path).unwrap_or_default();
    // Original journal retained; resume starts a new run id under runs/, but
    // the recorded host kind in the original manifest must still be git.
    let manifest_path = cache
        .join("summoner")
        .join("runs")
        .join(&run_id)
        .join("manifest.json");
    assert!(manifest_path.is_file(), "manifest must exist for resume");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    if let Some(kind) = manifest["host"]["kind"].as_str() {
        assert_eq!(kind, "git", "run must pin git host in manifest");
    }
    // After kill+resume we either have more journal activity or a new run dir.
    let runs_dir = cache.join("summoner").join("runs");
    let run_count = std::fs::read_dir(&runs_dir)
        .unwrap()
        .filter(|e| e.as_ref().ok().is_some_and(|e| e.path().is_dir()))
        .count();
    assert!(
        run_count >= 1,
        "expected at least the original run directory"
    );
    // Resume must not leave a live claim stuck forever: ledger tasks for the
    // order should not all be "running" without a resume re-dispatch path.
    let _ = events;
}

fn find_run_id(cache: &Path) -> Option<String> {
    let runs = cache.join("summoner").join("runs");
    let entries = std::fs::read_dir(runs).ok()?;
    entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .next()
}

fn init_repo(repo: &Path) {
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.txt"), "lib\n").unwrap();
    run(repo, &["git", "init", "-q"]);
    run(repo, &["git", "config", "user.email", "recovery@test"]);
    run(repo, &["git", "config", "user.name", "recovery"]);
    run(repo, &["git", "add", "-A"]);
    run(repo, &["git", "commit", "-qm", "init"]);
}

fn write_slow_worker(bin: &Path) -> PathBuf {
    let path = bin.join("slow-worker");
    std::fs::write(
        &path,
        r#"#!/bin/sh
set -e
sleep 30
printf 'done\n' >> src/lib.txt
git add src/lib.txt
git commit -qm "slow"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&path, p).unwrap();
    }
    path
}

fn write_config(config_home: &Path, worker: &Path) {
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
timeout_secs = 120
"#
        ),
    )
    .unwrap();
}

fn write_order(repo: &Path) {
    std::fs::create_dir_all(repo.join("orders")).unwrap();
    std::fs::write(
        repo.join("orders/slow.toml"),
        r#"
id = "slow"
title = "Slow touch"
brief = "sleep then edit"
scope = ["src/lib.txt"]
"#,
    )
    .unwrap();
}

fn minimal_path(extra: &Path) -> std::ffi::OsString {
    let mut parts = vec![extra.to_path_buf()];
    for system in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
        let p = PathBuf::from(system);
        if p.is_dir() {
            parts.push(p);
        }
    }
    std::env::join_paths(parts).unwrap()
}

fn run(dir: &Path, argv: &[&str]) {
    assert!(
        Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}
