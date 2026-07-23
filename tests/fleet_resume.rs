//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn resume_deduplicates_variant_siblings_sharing_one_order_file() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn f() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w",
        60,
    );
    let script = fixture.base.path().join("fake2-executor.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'pub fn g() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w2\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.append_config(&format!(
        "\n[executors.fake2]\nargv = [\"{}\"]\nprompt = \"stdin\"\ntimeout_secs = 60",
        script.display()
    ));
    let order = fixture.order(
        "race.toml",
        r#"
id = "race"
title = "t"
brief = "b"
scope = ["src"]
verify_profile = "fast"
variants = ["fake", "fake2"]
"#,
    );

    let report = fixture.run_report(&[&order], 0);
    let run_id = report["run_id"].as_str().unwrap();

    // Both siblings report the same order file; resume must not load it
    // twice (duplicate expanded ids would abort validation).
    let output = fixture.summoner(&["resume", run_id]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let resumed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(resumed["summary"]["verified"], 2, "{resumed}");
    for entry in resumed["orders"].as_array().unwrap() {
        assert!(
            entry["detail"]
                .as_str()
                .is_some_and(|d| d.contains("carried")),
            "{resumed}"
        );
    }
}

#[test]
fn resume_carries_successes_and_reruns_failures_on_their_branches() {
    require_grove();
    let fixture = Fixture::new(true);
    // Order "bad" fails until the flag file exists; "good" always succeeds.
    let flag = fixture.base.path().join("fixed");
    fixture.executor(
        &format!(
            "branch=$(git symbolic-ref --short HEAD)\n\
             case \"$branch\" in\n\
               *smn-bad) if [ ! -f {flag} ]; then echo 'SESSION=bad-session'; exit 3; fi\n\
                         test \"$1\" = --resume && test \"$2\" = bad-session || exit 4\n\
                         echo 'pub fn bad() {{}}' > src/bad.rs ;;\n\
               *) echo 'pub fn good() {{}}' > src/good.rs ;;\n\
             esac\n\
             git add -A\ngit commit -qm 'executor work'",
            flag = flag.display()
        ),
        60,
    );
    fixture.append_config(&format!(
        "session_marker = \"SESSION=\"\nresume_argv = [\"{}\", \"--resume\", \"{{session_id}}\"]",
        fixture.base.path().join("fake-executor.sh").display()
    ));
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
    // Recovery owns its inputs: remove both source orders and replace the
    // current executor/defaults with a backend that would fail if consulted.
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    std::fs::write(
        fixture.repo.join(".summoner.toml"),
        "default_executor = \"poison\"\nmax_parallel = 9\n\n\
         [executors.poison]\nargv = [\"false\"]\nprompt = \"stdin\"\n",
    )
    .unwrap();
    fixture.commit_all("replace current summoner config");
    let output = fixture.summoner_with_env(
        &["resume", run_id],
        &[("SUMMONER_MAX_PARALLEL", "7"), ("SUMMONER_REVISE", "4")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let second: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_resume_carry_report(&fixture, &second);
}

fn assert_resume_carry_report(fixture: &Fixture, second: &Value) {
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
    let bad = second["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "bad")
        .unwrap();
    assert_eq!(bad["session_id"], "bad-session", "{second}");
    assert_eq!(bad["attempts"], 2, "{second}");
    let show = Command::new("git")
        .args(["show", "grove/smn-bad:src/bad.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "{second}");
    let resumed_id = second["run_id"].as_str().unwrap();
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .base
                .path()
                .join("xdg/summoner/runs")
                .join(resumed_id)
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["settings"]["max_parallel"], 2, "{manifest}");
    assert_eq!(manifest["settings"]["revise"], 0, "{manifest}");
    assert!(manifest["backends"].get("fake").is_some(), "{manifest}");
    assert!(manifest["backends"].get("poison").is_none(), "{manifest}");
    for order in manifest["orders"].as_array().unwrap() {
        assert_eq!(order["roles"]["executor"], "fake", "{manifest}");
        assert!(
            order["source_path"]
                .as_str()
                .is_some_and(|path| path.contains("resume-orders")),
            "{manifest}"
        );
    }
}

#[test]
fn resume_fails_closed_when_the_recorded_executor_binary_drifts() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);
    let first = fixture.run_report(&[&order], 0);
    let run_id = first["run_id"].as_str().unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.base.path().join("fake-executor.sh"))
        .unwrap()
        .write_all(b"\n# upgraded\n")
        .unwrap();

    let resumed = fixture.summoner(&["resume", run_id]);
    assert_eq!(resumed.status.code(), Some(2));
    let error = String::from_utf8_lossy(&resumed.stderr);
    assert!(error.contains("executor binary drift"), "{error}");
    assert!(error.contains("start a new run"), "{error}");
}

#[test]
fn hard_kill_after_grove_finish_recovers_without_report_or_source_order() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn recovered() {}' > src/recovered.rs\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order(
        "recover.toml",
        "id = \"recover\"\ntitle = \"Recover\"\nbrief = \"Survive a hard kill.\"\n\
         scope = [\"src/recovered.rs\"]\nverify_profile = \"fast\"\n",
    );

    // Hold only the first release call after Grove finished + checkpoint.
    let wrapper = fixture.base.path().join("grove-release-barrier.sh");
    let blocked = fixture.base.path().join("release-blocked");
    let proceed = fixture.base.path().join("release-proceed");
    release_barrier_wrapper(&wrapper, &blocked, &proceed);
    let mut command =
        fixture.summoner_command(&["run", order.to_str().unwrap()], wrapper.to_str().unwrap());
    command
        .env("SUMMONER_REAL_GROVE", grove_bin())
        .env("SUMMONER_RELEASE_BLOCKED", &blocked)
        .env("SUMMONER_RELEASE_PROCEED", &proceed)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    wait_for(&blocked, 20);
    assert!(
        blocked.exists(),
        "Summoner never reached the release barrier"
    );

    let runs_root = fixture.base.path().join("xdg/summoner/runs");
    let runs: Vec<PathBuf> = std::fs::read_dir(&runs_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(runs.len(), 1, "expected exactly one interrupted run");
    let old_run = &runs[0];
    let journal = std::fs::read_to_string(old_run.join("events.jsonl")).unwrap();
    let checkpoint: Value = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .find(|event: &Value| event["event"] == "order_checkpoint")
        .expect("durable pre-release checkpoint");
    let worktree = PathBuf::from(checkpoint["report"]["worktree"].as_str().unwrap());
    assert!(!old_run.join("report.json").exists());

    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    std::fs::write(&proceed, "").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "the original run must be hard-killed"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while worktree.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !worktree.exists(),
        "Grove release did not complete after the kill"
    );

    // Neither mutable source orders nor current executor defaults participate
    // in recovery. The checkpoint and matching Grove receipt carry the work.
    std::fs::remove_file(&order).unwrap();
    std::fs::write(
        fixture.repo.join(".summoner.toml"),
        "default_executor = \"poison\"\n[executors.poison]\nargv = [\"false\"]\nprompt = \"stdin\"\n",
    )
    .unwrap();
    fixture.commit_all("replace current summoner config");
    let run_id = old_run.file_name().unwrap().to_str().unwrap();
    let resumed = fixture.summoner(&["resume", run_id]);
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    let report: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(report["summary"]["verified"], 1, "{report}");
    assert!(
        report["orders"][0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("carried from run")),
        "{report}"
    );

    // A resumed run is itself replayable even when every order was carried.
    let resumed_manifest: Value = serde_json::from_slice(
        &std::fs::read(
            runs_root
                .join(report["run_id"].as_str().unwrap())
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(resumed_manifest["orders"].as_array().unwrap().len(), 1);
    assert_eq!(resumed_manifest["orders"][0]["expanded"]["id"], "recover");
}

#[test]
fn resume_refuses_to_duplicate_a_nonterminal_grove_task() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn held() {}' > src/held.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order(
        "held.toml",
        "id = \"held\"\ntitle = \"Held\"\nbrief = \"Hold before execution.\"\n\
         scope = [\"src/held.rs\"]\nverify_profile = \"fast\"\n",
    );
    let wrapper = fixture.base.path().join("grove-exec-barrier.sh");
    let blocked = fixture.base.path().join("exec-blocked");
    let proceed = fixture.base.path().join("exec-proceed");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\n\
         if [ \"$1\" = task ] && [ \"$2\" = exec ]; then\n\
           : > \"$SUMMONER_EXEC_BLOCKED\"\n\
           while [ ! -e \"$SUMMONER_EXEC_PROCEED\" ]; do sleep 0.05; done\n\
         fi\n\
         exec \"$SUMMONER_REAL_GROVE\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command =
        fixture.summoner_command(&["run", order.to_str().unwrap()], wrapper.to_str().unwrap());
    command
        .env("SUMMONER_REAL_GROVE", grove_bin())
        .env("SUMMONER_EXEC_BLOCKED", &blocked)
        .env("SUMMONER_EXEC_PROCEED", &proceed)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !blocked.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(blocked.exists(), "Summoner never reached the exec barrier");

    let runs_root = fixture.base.path().join("xdg/summoner/runs");
    let old_run = std::fs::read_dir(&runs_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let journal = std::fs::read_to_string(old_run.join("events.jsonl")).unwrap();
    let dispatched: Value = journal
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .find(|event: &Value| event["event"] == "order_dispatched")
        .expect("dispatch record");
    let task_id = dispatched["task_id"].as_str().unwrap();
    let run_id = old_run.file_name().unwrap().to_str().unwrap();
    let duplicate = fixture.summoner(&["resume", run_id]);
    assert_eq!(duplicate.status.code(), Some(2));
    let error = String::from_utf8_lossy(&duplicate.stderr);
    assert!(error.contains("still owns Grove task"), "{error}");
    assert!(error.contains(task_id), "{error}");
    let status: Value =
        serde_json::from_slice(&fixture.grove(&["task", "status", "--json"]).stdout).unwrap();
    assert_eq!(status["tasks"].as_array().unwrap().len(), 1, "{status}");

    std::fs::write(&proceed, "").unwrap();
    let original = child.wait_with_output().unwrap();
    assert_eq!(
        original.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&original.stdout),
        String::from_utf8_lossy(&original.stderr)
    );
}

#[test]
fn resume_fails_closed_when_green_journal_evidence_contradicts_grove() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn contradiction() {}' > src/contradiction.rs\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order(
        "contradiction.toml",
        "id = \"contradiction\"\ntitle = \"Contradiction\"\nbrief = \"Create work.\"\n\
         scope = [\"src/contradiction.rs\"]\nverify_profile = \"fast\"\n",
    );
    let report = fixture.run_report(&[&order], 0);
    let run_id = report["run_id"].as_str().unwrap();
    let journal_path = fixture
        .base
        .path()
        .join("xdg/summoner/runs")
        .join(run_id)
        .join("events.jsonl");
    let mut records: Vec<Value> = std::fs::read_to_string(&journal_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let terminal = records
        .iter_mut()
        .find(|record| record["event"] == "order_finished")
        .unwrap();
    terminal["report"]["finish"]["verified"] = Value::Bool(false);
    let text = records
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n")
        + "\n";
    std::fs::write(&journal_path, text).unwrap();

    let resumed = fixture.summoner(&["resume", run_id]);
    assert_eq!(resumed.status.code(), Some(2));
    let error = String::from_utf8_lossy(&resumed.stderr);
    assert!(
        error.contains("disagrees with Grove verification"),
        "{error}"
    );
}

#[test]
fn resume_rejects_an_approval_bound_to_a_different_grove_source() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn approved() {}' > src/approved.rs\n\
         git add -A\ngit commit -qm 'executor work'",
        60,
    );
    fixture.reviewer("echo '{\"verdict\":\"approve\",\"findings\":[]}'");
    let order = fixture.order(
        "approved.toml",
        "id = \"approved\"\ntitle = \"Approved\"\nbrief = \"Create work.\"\n\
         scope = [\"src/approved.rs\"]\nverify_profile = \"fast\"\n",
    );
    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["orders"][0]["outcome"], "approved", "{report}");
    let run_id = report["run_id"].as_str().unwrap();

    // Rebind the journaled approval to a wrong snapshot digest so resume must
    // refuse to carry green work (same agree() path as a Grove source mismatch).
    let journal = fixture
        .base
        .path()
        .join("xdg/summoner/runs")
        .join(run_id)
        .join("events.jsonl");
    assert!(journal.is_file(), "missing journal {}", journal.display());
    let text = std::fs::read_to_string(&journal).expect("read journal");
    let wrong = "0".repeat(64);
    let mut rewritten = String::new();
    for line in text.lines() {
        if let Some(idx) = line.find("candidate_snapshot_sha256") {
            // Replace the first 64-hex run after the key with zeros.
            let (head, tail) = line.split_at(idx);
            let mut out = String::from(head);
            let mut replaced = false;
            let mut chars = tail.chars().peekable();
            while let Some(c) = chars.next() {
                out.push(c);
                if !replaced && c == '"' {
                    // after a quote, look for 64 hex
                    let mut hex = String::new();
                    while hex.len() < 64 {
                        match chars.peek().copied() {
                            Some(h) if h.is_ascii_hexdigit() => {
                                hex.push(h);
                                chars.next();
                            }
                            _ => break,
                        }
                    }
                    if hex.len() == 64 {
                        out.push_str(&wrong);
                        replaced = true;
                    } else {
                        out.push_str(&hex);
                    }
                }
            }
            rewritten.push_str(&out);
        } else {
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    assert!(
        rewritten.contains(&wrong),
        "failed to rebind candidate_snapshot_sha256 in {journal:?}"
    );
    std::fs::write(&journal, rewritten).expect("rewrite journal");

    let resumed = fixture
        .summoner_command(&["resume", run_id], &grove_bin())
        .output()
        .unwrap();
    assert_eq!(
        resumed.status.code(),
        Some(2),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr)
    );
    let error = String::from_utf8_lossy(&resumed.stderr);
    assert!(error.contains("approval snapshot"), "{error}");
    assert!(
        error.contains("disagrees with Grove task source"),
        "{error}"
    );
}
