#![allow(dead_code)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

pub struct Fixture {
    pub temp: TempDir,
    pub repo: PathBuf,
    pub executor: PathBuf,
}

impl Fixture {
    pub fn new(body: &str) -> Self {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("README.md"), "fixture\n").unwrap();
        std::fs::write(repo.join("src/.keep"), "fixture\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "summoner-test"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let executor = temp.path().join("executor.sh");
        std::fs::write(&executor, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&executor, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            repo.join(".summoner.toml"),
            format!(
                "default_executor = \"fake\"\nmax_parallel = 2\n\n[executors.fake]\nargv = [\"{}\"]\nprompt = \"stdin\"\ntimeout_secs = 10\n\n[verification.profiles.fast]\ncommands = [{{ argv = [\"sh\", \"-c\", \"test \\\"$(git rev-parse HEAD)\\\" != \\\"{{base}}\\\"\"] }}]\n",
                executor.display()
            ),
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "config"]);
        Self {
            temp,
            repo,
            executor,
        }
    }

    pub fn order(&self, name: &str, body: &str) -> PathBuf {
        let path = self.temp.path().join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    pub fn write_config(&self, text: &str) {
        std::fs::write(self.repo.join(".summoner.toml"), text).unwrap();
    }

    pub fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_summoner"))
            .args(args)
            .current_dir(&self.repo)
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .output()
            .unwrap()
    }

    pub fn run_json(&self, order_paths: &[PathBuf], extra: &[&str]) -> (Output, Value) {
        let mut args = vec!["run"];
        args.extend_from_slice(extra);
        args.extend(order_paths.iter().map(|path| path.to_str().unwrap()));
        let output = self.run(&args);
        let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{}\n{}\n{error}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, json)
    }
}

pub fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn git_text(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
