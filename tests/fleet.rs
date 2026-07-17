//! Fleet integration against a real grove binary: fake shell executors, real
//! worktrees, claims, receipts, and the ranked report. Runtime-skipped when a
//! grove >= 0.3.2 is not available (point SUMMONER_TEST_GROVE at one).
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
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

    /// Extra config lines appended to `.summoner.toml`. Top-level keys must be
    /// appended via this BEFORE `executor()` writes the executor table; lines
    /// appended after land inside `[executors.fake]`, which suits per-executor
    /// keys like `usage_marker`.
    fn append_config(&self, lines: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(self.repo.join(".summoner.toml"))
            .unwrap();
        writeln!(file, "{lines}").unwrap();
        drop(file);
        self.commit_all("config tweak");
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
fn sigterm_tears_down_the_fleet_and_still_emits_a_partial_report() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor("sleep 30", 60);
    let order = fixture.order(
        "slow.toml",
        "id = \"slow\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/slow.rs\"]\nverify_profile = \"fast\"\n",
    );

    let stdout_path = fixture.base.path().join("stream.ndjson");
    let mut summoner = Command::new(SUMMONER)
        .args(["run", "--stream", order.to_str().unwrap()])
        .current_dir(&fixture.repo)
        .env("SUMMONER_GROVE_BIN", grove_bin())
        .env("GROVE_CACHE_ROOT", fixture.base.path().join("cache"))
        .env("XDG_CACHE_HOME", fixture.base.path().join("xdg"))
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Interrupt only once the executor is actually running.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let streamed = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        if streamed.contains("order_dispatched") {
            break;
        }
        assert!(Instant::now() < deadline, "order never dispatched");
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &summoner.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let exit = summoner.wait().unwrap();
    assert_eq!(exit.code(), Some(1), "interrupted run needs review");

    // The stream still ends with a complete report classifying the interrupt.
    let streamed = std::fs::read_to_string(&stdout_path).unwrap();
    let last: Value = serde_json::from_str(streamed.lines().last().unwrap()).unwrap();
    assert_eq!(last["event"], "report", "{streamed}");
    let entry = &last["report"]["orders"][0];
    assert_eq!(entry["outcome"], "interrupted", "{streamed}");

    // No leaked claim, and the worktree came back.
    assert_eq!(
        fixture.task_states(),
        [("smn-slow".into(), "abandoned".into())]
    );
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(!Path::new(worktree).exists(), "worktree released");
}

#[test]
fn fail_fast_skips_the_remaining_queue_after_the_threshold() {
    require_grove!();
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
    require_grove!();
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

#[test]
fn resume_carries_successes_and_reruns_failures_on_their_branches() {
    require_grove!();
    let fixture = Fixture::new(true);
    // Order "bad" fails until the flag file exists; "good" always succeeds.
    let flag = fixture.base.path().join("fixed");
    fixture.executor(
        &format!(
            "branch=$(git symbolic-ref --short HEAD)\n\
             case \"$branch\" in\n\
               *smn-bad) test -f {flag} || exit 3\n\
                         echo 'pub fn bad() {{}}' > src/bad.rs ;;\n\
               *) echo 'pub fn good() {{}}' > src/good.rs ;;\n\
             esac\n\
             git add -A\ngit commit -qm 'executor work'",
            flag = flag.display()
        ),
        60,
    );
    let a = fixture.order(
        "a.toml",
        "id = \"good\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/good.rs\"]\nverify_profile = \"fast\"\n",
    );
    let b = fixture.order(
        "b.toml",
        "id = \"bad\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/bad.rs\"]\nverify_profile = \"fast\"\n",
    );

    let first = fixture.run_report(&[&a, &b], 1);
    assert_eq!(first["summary"]["verified"], 1, "{first}");
    assert_eq!(first["summary"]["executor_failed"], 1, "{first}");
    let run_id = first["run_id"].as_str().unwrap();

    std::fs::write(&flag, "").unwrap();
    let output = fixture.summoner(&["resume", run_id]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let second: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(second["summary"]["verified"], 2, "{second}");
    let good = second["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "good")
        .unwrap();
    assert!(
        good["detail"]
            .as_str()
            .is_some_and(|d| d.contains("carried from run")),
        "{second}"
    );
    // The re-run continued the original branch.
    let show = Command::new("git")
        .args(["show", "grove/smn-bad:src/bad.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "{second}");
}

#[test]
fn streamed_run_emits_ndjson_events_and_a_final_report_line() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);

    let output = fixture.summoner(&["run", "--stream", order.to_str().unwrap()]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Every stdout line is one JSON object: the whole point of the stream.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect();
    let names: Vec<&str> = lines
        .iter()
        .map(|line| line["event"].as_str().unwrap())
        .collect();
    assert_eq!(names.first(), Some(&"run_started"), "{names:?}");
    for expected in [
        "order_started",
        "order_dispatched",
        "order_exec_done",
        "order_verify",
        "order_finished",
        "run_finished",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    assert_eq!(names.last(), Some(&"report"), "{names:?}");
    let report = &lines.last().unwrap()["report"];
    assert_eq!(report["orders"][0]["outcome"], "verified");

    // order_dispatched carries what a live consumer needs to follow along.
    let dispatched = lines
        .iter()
        .find(|line| line["event"] == "order_dispatched")
        .unwrap();
    assert!(dispatched["task_id"].is_string());
    assert!(dispatched["stdout_log"].is_string());

    // The sidecar log always exists, stream or not, and ends with run_finished.
    let run_dir = lines[0]["run_dir"].as_str().unwrap();
    let sidecar = std::fs::read_to_string(Path::new(run_dir).join("events.jsonl")).unwrap();
    let sidecar_names: Vec<String> = sidecar
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["event"].to_string())
        .collect();
    assert_eq!(sidecar_names.last().unwrap(), "\"run_finished\"");
    assert!(!sidecar.contains("\"event\":\"report\""));
}

#[test]
fn branch_switching_executor_downgrades_success_to_error() {
    require_grove!();
    let fixture = Fixture::new(true);
    // In-scope work, receipts green — but the checkout leaves its leased
    // branch, so grove refuses the release. That worktree is leaked, and the
    // run must not call it a success.
    fixture.executor(
        "git checkout -qb rogue\necho 'pub fn r() {}' > src/rogue.rs\n\
         git add -A\ngit commit -qm rogue",
        60,
    );
    let order = fixture.order(
        "rogue.toml",
        r#"
id = "rogue"
title = "Wanders off its branch"
brief = "Commit in scope but on a different branch."
scope = ["src"]
verify_profile = "fast"
"#,
    );

    let report = fixture.run_report(&[&order], 1);
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "error", "{report}");
    assert!(
        entry["release_error"]
            .as_str()
            .is_some_and(|e| e.contains("manual recovery")),
        "{report}"
    );
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(
        Path::new(worktree).exists(),
        "leaked worktree still on disk"
    );
}

#[test]
fn dependent_order_builds_on_its_dependency_branch() {
    require_grove!();
    let fixture = Fixture::new(true);
    // The second order proves it saw the first order's work: it refuses to
    // proceed unless src/one.rs (committed by order one) is present.
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) echo 'pub fn one() {}' > src/one.rs ;;\n\
           *smn-two) test -f src/one.rs || exit 9\n\
                     echo 'pub fn two() {}' > src/two.rs ;;\n\
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
title = "Build on file one"
brief = "Write src/two.rs next to src/one.rs and commit."
scope = ["src/two.rs"]
verify_profile = "fast"
after = ["one"]
base = "grove/smn-one"
"#,
    );

    let report = fixture.run_report(&[&a, &b], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    let two = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "two")
        .unwrap();
    assert_eq!(two["after"], serde_json::json!(["one"]));
    // Both files exist on order two's branch: the chain composed.
    let show = Command::new("git")
        .args(["show", "grove/smn-two:src/one.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "src/one.rs missing on smn-two");
}

#[test]
fn dependents_of_a_failed_order_are_skipped() {
    require_grove!();
    let fixture = Fixture::new(true);
    fixture.executor(
        "branch=$(git symbolic-ref --short HEAD)\n\
         case \"$branch\" in\n\
           *smn-one) exit 3 ;;\n\
           *) echo 'pub fn two() {}' > src/two.rs\n\
              git add -A\ngit commit -qm 'executor work' ;;\n\
         esac",
        60,
    );
    let a = fixture.order(
        "a.toml",
        r#"
id = "one"
title = "Fail"
brief = "Exit 3."
scope = ["src/one.rs"]
"#,
    );
    let b = fixture.order(
        "b.toml",
        r#"
id = "two"
title = "Never runs"
brief = "Should be skipped."
scope = ["src/two.rs"]
after = ["one"]
"#,
    );

    let report = fixture.run_report(&[&a, &b], 1);
    assert_eq!(report["summary"]["executor_failed"], 1, "{report}");
    assert_eq!(report["summary"]["skipped"], 1, "{report}");
    let two = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "two")
        .unwrap();
    assert_eq!(two["outcome"], "skipped");
    assert!(
        two["detail"]
            .as_str()
            .is_some_and(|d| d.contains("\"one\"") && d.contains("executor_failed")),
        "{report}"
    );
    // The skipped order never began a task: only one grove task exists.
    assert_eq!(fixture.task_states().len(), 1);
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
