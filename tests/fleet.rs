//! Fleet integration against a real grove binary: fake shell executors, real
//! worktrees, claims, receipts, and the ranked report. Runtime-skipped when a
//! grove >= 0.3.2 is not available (point SUMMONER_TEST_GROVE at one).
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

fn grove_bin() -> String {
    std::env::var("SUMMONER_TEST_GROVE").unwrap_or_else(|_| "grove".to_string())
}

fn grove_available() -> bool {
    let Some(version) = Command::new(grove_bin())
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    else {
        return false;
    };
    let numbers: Vec<u64> = version
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(3)
        .collect();
    (numbers[0], numbers[1], numbers[2]) >= (0, 3, 2)
}

macro_rules! require_grove {
    () => {
        if !grove_available() {
            eprintln!("skipping: grove >= 0.3.2 not on PATH (set SUMMONER_TEST_GROVE)");
            return;
        }
    };
}

struct Fixture {
    base: TempDir,
    repo: PathBuf,
    with_verification: bool,
}

const GROVE_TOML: &str = r#"[verification]
required = ["fast"]

[verification.profiles.fast]
continue_on_failure = false
commands = [{ argv = ["true"], allow_zero_tests = true }]
"#;

impl Fixture {
    /// A committed cargo package with git identity; verification profile and
    /// summoner config are committed so they predate every scope snapshot.
    fn new(with_verification: bool) -> Fixture {
        let base = TempDir::new().unwrap();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname='p'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        std::fs::write(repo.join("src/lib.rs"), "").unwrap();
        if with_verification {
            std::fs::write(repo.join(".grove.toml"), GROVE_TOML).unwrap();
        }
        let fixture = Fixture {
            base,
            repo,
            with_verification,
        };
        fixture.git(&["init", "-q"]);
        fixture.git(&["config", "user.email", "t@example.com"]);
        fixture.git(&["config", "user.name", "fleet-test"]);
        fixture
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.repo)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// One fake executor; the config routes the prompt on stdin, which the
    /// scripts deliberately ignore (prompt composition is unit-tested).
    fn executor(&self, body: &str, timeout_secs: u64) {
        let script = self.base.path().join("fake-executor.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Only point at a default profile the repository actually declares.
        let default_profile = if self.with_verification {
            "default_verify_profile = \"fast\"\n"
        } else {
            ""
        };
        std::fs::write(
            self.repo.join(".summoner.toml"),
            format!(
                "default_executor = \"fake\"\n{default_profile}\
                 max_parallel = 2\n\n[executors.fake]\nargv = [\"{}\"]\nprompt = \"stdin\"\n\
                 timeout_secs = {timeout_secs}\n",
                script.display()
            ),
        )
        .unwrap();
        self.commit_all("fixture");
    }

    fn order(&self, name: &str, body: &str) -> PathBuf {
        let orders = self.base.path().join("orders");
        std::fs::create_dir_all(&orders).unwrap();
        let path = orders.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn summoner(&self, args: &[&str]) -> Output {
        Command::new(SUMMONER)
            .args(args)
            .current_dir(&self.repo)
            .env("SUMMONER_GROVE_BIN", grove_bin())
            .env("GROVE_CACHE_ROOT", self.base.path().join("cache"))
            .env("XDG_CACHE_HOME", self.base.path().join("xdg"))
            .output()
            .expect("run summoner")
    }

    fn grove(&self, args: &[&str]) -> Output {
        Command::new(grove_bin())
            .args(args)
            .current_dir(&self.repo)
            .env("GROVE_CACHE_ROOT", self.base.path().join("cache"))
            .output()
            .expect("run grove")
    }

    fn run_report(&self, order_paths: &[&Path], expect_exit: i32) -> Value {
        let mut args = vec!["run"];
        args.extend(order_paths.iter().map(|p| p.to_str().unwrap()));
        let output = self.summoner(&args);
        assert_eq!(
            output.status.code(),
            Some(expect_exit),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("report is JSON")
    }

    fn task_states(&self) -> Vec<(String, String)> {
        let output = self.grove(&["task", "status", "--json"]);
        assert!(output.status.success());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| {
                (
                    task["owner"].as_str().unwrap_or_default().to_string(),
                    task["status"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }
}

const ORDER_TOML: &str = r#"
id = "wave"
title = "Add the wave function"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
acceptance = ["src/lib.rs gains a function", "work is committed"]
verify_profile = "fast"
"#;

#[test]
fn happy_path_order_is_verified_released_and_salvaged_to_its_branch() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 0);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "verified", "{report}");
    assert_eq!(entry["executor_exit"], 0);
    assert_eq!(entry["commits"], 1);
    assert_eq!(entry["finish"]["verified"], true);
    assert_eq!(report["summary"]["verified"], 1);

    // The worktree is gone; the work survives on the order's branch.
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(!Path::new(worktree).exists(), "worktree released");
    let show = Command::new("git")
        .args(["show", "grove/smn-wave:src/lib.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&show.stdout).contains("pub fn wave()"));

    // No claim leaked: the task is terminal.
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "finished".into())]
    );
}

#[test]
fn conflicting_scope_reports_blocked_without_dispatching() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor("exit 0", 60);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let held = fixture.grove(&["claim", "--agent", "blocker", "--task", "hold", "src"]);
    assert!(
        held.status.success(),
        "{}",
        String::from_utf8_lossy(&held.stderr)
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "blocked", "{report}");
    assert!(entry["conflicts"].as_array().is_some_and(|c| !c.is_empty()));
    assert!(entry["task_id"].is_null(), "no task was begun");
}

#[test]
fn sleeping_executor_is_stalled_by_the_grove_deadline_and_abandoned() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor("sleep 30", 2);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "stalled", "{report}");
    assert_eq!(entry["executor_exit"], 124);
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}

#[test]
fn failing_executor_skips_verification_and_is_abandoned() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor("exit 3", 60);
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "executor_failed", "{report}");
    assert_eq!(entry["executor_exit"], 3);
    assert!(entry.get("verify").is_none(), "verification skipped");
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}

#[test]
fn out_of_scope_write_is_a_scope_violation() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\necho outside > outside.txt\n\
         git add -A\ngit commit -qm 'overreach'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "scope_violation", "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("outside.txt")),
        "{report}"
    );
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "abandoned".into())]
    );
}

#[test]
fn repo_without_required_profiles_completes_with_the_override_recorded() {
    require_grove!();
    let fixture = Fixture::new(false);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order(
        "wave.toml",
        r#"
id = "wave"
title = "Add the wave function"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
"#,
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "completed", "{report}");
    assert!(
        entry["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("no required verification profiles"))
    );
    assert_eq!(
        fixture.task_states(),
        [("smn-wave".into(), "finished".into())]
    );
}

#[test]
fn two_independent_orders_run_in_one_fleet_and_both_verify() {
    require_grove!();
    let fixture = Fixture::new(true);
    // One script serves both orders: the worktree branch names the order, and
    // each order writes only its own declared file.
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) echo 'pub fn one() {}' > src/one.rs ;;\n\
           *smn-two) echo 'pub fn two() {}' > src/two.rs ;;\n\
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
title = "Touch file two"
brief = "Write src/two.rs and commit."
scope = ["src/two.rs"]
verify_profile = "fast"
"#,
    );

    let report = fixture.run_report(&[&a, &b], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    let states = fixture.task_states();
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|(_, status)| status == "finished"));
}
