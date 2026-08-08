#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use support::{Fixture, git, git_text};

fn order(id: &str, scope: &str, extra: &str) -> String {
    format!(
        "id = \"{id}\"\ntitle = \"{id}\"\nbrief = \"do work\"\nscope = [\"{scope}\"]\nverify_profile = \"fast\"\n{extra}\n"
    )
}

#[test]
fn scope_dot_accepts_multiple_files_and_verifies_committed_head() {
    let fixture = Fixture::new(
        "printf one > src/one.rs\nprintf two > src/two.rs\ngit add .\ngit commit -qm work",
    );
    let path = fixture.order("all.toml", &order("all", ".", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["summary"]["verified"], 1, "{report}");
    assert_eq!(
        report["orders"][0]["changed_paths"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        report["orders"][0]["candidate_commit"]
            .as_str()
            .unwrap()
            .len()
            >= 7
    );
    assert!(
        report["orders"][0]["verify"][0]["success"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn sibling_prefix_and_parent_traversal_are_refused() {
    let sibling_fixture =
        Fixture::new("printf x > src/file-other.rs\ngit add .\ngit commit -qm sibling");
    let sibling = sibling_fixture.order("bad.toml", &order("bad", "src/file", ""));
    let (sibling_output, sibling_report) = sibling_fixture.run_json(&[sibling], &[]);
    assert_eq!(sibling_output.status.code(), Some(1), "{sibling_report}");
    assert_eq!(sibling_report["orders"][0]["outcome"], "scope_violation");

    let traversal_fixture = Fixture::new("true");
    let traversal = traversal_fixture.order("walk.toml", &order("walk", "../outside", ""));
    let traversal_out = traversal_fixture.run(&["check", traversal.to_str().unwrap()]);
    assert_eq!(traversal_out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&traversal_out.stderr).contains("parent directory"));
}

#[test]
fn strict_config_rejects_unknown_fields() {
    let fixture = Fixture::new("true");
    let config = std::fs::read_to_string(fixture.repo.join(".summoner.toml")).unwrap();
    fixture.write_config(&format!("{config}\nunknown = true\n"));
    let path = fixture.order("unknown.toml", &order("unknown", ".", ""));
    let output = fixture.run(&["check", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown"));
}

#[test]
fn executor_argv_expands_absolute_git_common_dir() {
    let fixture = Fixture::new("true");
    fixture.write_config(&format!(
        "default_executor = \"fake\"\ndefault_verify_profile = \"fast\"\n[executors.fake]\nargv = [\"{}\", \"{{git_common_dir}}\", \"{{worktree}}\"]\nprompt = \"arg\"\n[verification.profiles.fast]\ncommands = [[\"true\"]]\n",
        fixture.executor.display()
    ));
    let path = fixture.order("common-dir.toml", &order("common", ".", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let argv = report["orders"][0]["executor"]["argv"].as_array().unwrap();
    let actual = argv[1].as_str().unwrap();
    let raw = git_text(&fixture.repo, &["rev-parse", "--git-common-dir"]);
    let expected = if Path::new(&raw).is_absolute() {
        Path::new(&raw).to_path_buf()
    } else {
        fixture.repo.join(raw)
    }
    .canonicalize()
    .unwrap();
    assert_eq!(Path::new(actual), expected);
    assert!(Path::new(actual).is_absolute());
    assert!(
        !argv
            .iter()
            .any(|arg| arg.as_str().unwrap().contains("{git_common_dir}"))
    );
}

#[test]
fn doctor_checks_only_order_selected_entries() {
    let fixture = Fixture::new("true");
    fixture.write_config(
        "default_executor = \"selected\"\ndefault_verify_profile = \"selected\"\n\n[executors.selected]\nargv = [\"true\"]\nprompt = \"arg\"\n\n[executors.missing]\nargv = [\"summoner-test-missing-executor\"]\nprompt = \"arg\"\nenv_required = [\"SUMMONER_TEST_MISSING_EXECUTOR_ENV_9F2E\"]\n\n[verification.profiles.selected]\ncommands = [[\"true\"]]\n\n[verification.profiles.missing]\ncommands = [[\"summoner-test-missing-verifier\"]]\n",
    );
    let path = fixture.order(
        "selected.toml",
        "id = \"selected\"\ntitle = \"selected\"\nbrief = \"check selected tools\"\nscope = [\"README.md\"]\n",
    );

    let selected = fixture.run(&["doctor", path.to_str().unwrap()]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected_report: serde_json::Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected_report["executors"].as_array().unwrap().len(), 1);
    assert_eq!(selected_report["executors"][0]["name"], "selected");
    assert_eq!(selected_report["verification"].as_array().unwrap().len(), 1);
    assert_eq!(selected_report["verification"][0]["name"], "selected[0]");

    let full = fixture.run(&["doctor"]);
    assert_eq!(full.status.code(), Some(1));
    let full_report: serde_json::Value = serde_json::from_slice(&full.stdout).unwrap();
    assert_eq!(full_report["executors"].as_array().unwrap().len(), 2);
    assert!(
        full_report["executors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "missing" && !entry["available"].as_bool().unwrap())
    );
    assert_eq!(full_report["verification"].as_array().unwrap().len(), 2);
    assert!(
        full_report["verification"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["name"] == "missing[0]" && !entry["available"].as_bool().unwrap()
            })
    );
}

#[test]
fn rename_scope_reports_both_sides_without_following_rename() {
    let fixture = Fixture::new("mv src/old.rs src/new.rs\ngit add -A\ngit commit -qm rename");
    std::fs::write(fixture.repo.join("src/old.rs"), "old\n").unwrap();
    git(&fixture.repo, &["add", "."]);
    git(&fixture.repo, &["commit", "-qm", "old"]);
    let path = fixture.order("rename.toml", &order("rename", "src/new.rs", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["orders"][0]["outcome"], "scope_violation");
    let changed = report["orders"][0]["changed_paths"].as_array().unwrap();
    assert!(changed.iter().any(|path| path == "src/old.rs"), "{report}");
    assert!(changed.iter().any(|path| path == "src/new.rs"), "{report}");
}

#[test]
fn invalid_base_reference_is_rejected_before_worktree_creation() {
    let fixture = Fixture::new("true");
    let before = git_text(&fixture.repo, &["worktree", "list", "--porcelain"]);
    let path = fixture.order(
        "invalid-base.toml",
        &order("invalid", ".", "base = \"missing-ref\""),
    );
    let output = fixture.run(&["run", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid base"));
    assert_eq!(
        git_text(&fixture.repo, &["worktree", "list", "--porcelain"]),
        before
    );
}

#[test]
fn option_like_base_reference_is_rejected_without_worktree_side_effects() {
    let fixture = Fixture::new("true");
    let before = git_text(&fixture.repo, &["worktree", "list", "--porcelain"]);
    let path = fixture.order(
        "option-base.toml",
        &order("optionbase", ".", "base = \"-evil\""),
    );
    let output = fixture.run(&["run", path.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not begin"));
    assert_eq!(
        git_text(&fixture.repo, &["worktree", "list", "--porcelain"]),
        before
    );
}

#[test]
fn invalid_explicit_branches_are_rejected_before_worktree_creation() {
    for (id, branch) in [("optionbranch", "-evil"), ("badbranch", "bad..branch")] {
        let fixture = Fixture::new("true");
        let before = git_text(&fixture.repo, &["worktree", "list", "--porcelain"]);
        let path = fixture.order(
            "invalid-branch.toml",
            &order(id, ".", &format!("branch = \"{branch}\"")),
        );
        let output = fixture.run(&["run", path.to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(2), "{branch}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("branch"),
            "{branch}"
        );
        assert_eq!(
            git_text(&fixture.repo, &["worktree", "list", "--porcelain"]),
            before,
            "{branch}"
        );
    }
}

#[test]
fn base_report_keeps_the_resolved_commit_when_the_ref_moves() {
    let fixture = Fixture::new(
        "printf moved > src/moved.rs\ngit add .\ngit commit -qm moved\nref=$(git rev-parse --git-path refs/heads/moving-base)\nprintf '%s\\n' \"$(git rev-parse HEAD~2)\" > \"$ref\"",
    );
    git(&fixture.repo, &["branch", "moving-base"]);
    let initial = git_text(&fixture.repo, &["rev-parse", "moving-base"]);
    let path = fixture.order(
        "moving.toml",
        &order("moving", "src/moved.rs", "base = \"moving-base\""),
    );
    let (output, report) = fixture.run_json(&[path], &[]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["orders"][0]["base_commit"], initial);
    assert_ne!(
        git_text(&fixture.repo, &["rev-parse", "moving-base"]),
        initial
    );
}

#[test]
fn executor_failure_salvages_in_scope_changes_to_returned_branch() {
    let fixture =
        Fixture::new("printf failed > src/failure.rs\ngit add .\ngit commit -qm partial\nexit 7");
    let path = fixture.order("fail.toml", &order("fail", "src/failure.rs", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "executor_failed");
    let branch = entry["branch"].as_str().unwrap();
    let show = Command::new("git")
        .args(["show", &format!("{branch}:src/failure.rs")])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "in-scope edit was lost");
}

#[test]
fn timeout_kills_the_executor_process_tree() {
    let fixture = Fixture::new("");
    let pid_file = fixture.temp.path().join("descendant.pid");
    std::fs::write(
        &fixture.executor,
        format!(
            "#!/bin/sh\nsleep 30 & echo $! > '{}'\nwait\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture.executor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = fixture.order("slow.toml", &order("slow", ".", "timeout_secs = 2"));
    let started = Instant::now();
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["orders"][0]["outcome"], "timeout");
    assert!(started.elapsed() < Duration::from_secs(4));
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap_or_else(|error| panic!("missing descendant pid: {error}; {report}"))
        .trim()
        .parse()
        .unwrap();
    let stopped = (0..20).any(|_| {
        let stopped = !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success();
        if !stopped {
            std::thread::sleep(Duration::from_millis(50));
        }
        stopped
    });
    assert!(stopped, "descendant process {pid} survived timeout");
}

#[test]
fn direct_executor_exit_kills_descendants() {
    let fixture = Fixture::new("");
    let pid_file = fixture.temp.path().join("early-descendant.pid");
    std::fs::write(
        &fixture.executor,
        format!(
            "#!/bin/sh\nsleep 30 & echo $! > '{}'\nexit 0\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture.executor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = fixture.order("early.toml", &order("early", ".", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    assert_eq!(report["orders"][0]["outcome"], "unverified");
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap_or_else(|error| panic!("missing descendant pid: {error}; {report}"))
        .trim()
        .parse()
        .unwrap();
    let stopped = (0..20).any(|_| {
        let stopped = !Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success();
        if !stopped {
            std::thread::sleep(Duration::from_millis(50));
        }
        stopped
    });
    assert!(stopped, "descendant process {pid} survived direct exit");
}

#[test]
fn independent_orders_are_bounded_and_parent_starts_from_candidate() {
    let fixture = Fixture::new(
        "case \"$SUMMONER_ORDER\" in one) printf one > src/one.rs;; two) printf two > src/two.rs;; child) test -f src/one.rs && printf child > src/child.rs;; esac\ngit add .\ngit commit -qm work",
    );
    let one = fixture.order("one.toml", &order("one", "src/one.rs", ""));
    let two = fixture.order("two.toml", &order("two", "src/two.rs", ""));
    let child = fixture.order(
        "child.toml",
        &order("child", "src/child.rs", "after = [\"one\"]"),
    );
    let started = Instant::now();
    let (output, report) = fixture.run_json(&[one, two, child], &["--jobs", "2"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["summary"]["verified"], 3, "{report}");
    assert!(started.elapsed() < Duration::from_secs(5));
    let child_branch = report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "child")
        .unwrap()["branch"]
        .as_str()
        .unwrap()
        .to_owned();
    let parent = Command::new("git")
        .args(["show", &format!("{child_branch}:src/one.rs")])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(
        parent.status.success(),
        "child did not contain parent candidate"
    );
}

#[test]
fn independent_orders_actually_overlap_at_the_parallelism_barrier() {
    let fixture = Fixture::new("true");
    let barrier = fixture.temp.path().join("barrier");
    let release = fixture.temp.path().join("release");
    std::fs::write(
        &fixture.executor,
        format!(
            "#!/bin/sh\ncase \"$SUMMONER_ORDER\" in\none|two)\nprintf '%s\\n' \"$SUMMONER_ORDER\" >> '{}'\nif [ \"$(wc -l < '{}')\" -ge 2 ]; then : > '{}'; fi\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nprintf '%s' \"$SUMMONER_ORDER\" > src/$SUMMONER_ORDER.rs\n;;\nchild) printf child > src/child.rs;;\nesac\ngit add .\ngit commit -qm work\n",
            barrier.display(),
            barrier.display(),
            release.display(),
            release.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture.executor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let one = fixture.order("one.toml", &order("one", "src/one.rs", ""));
    let two = fixture.order("two.toml", &order("two", "src/two.rs", ""));
    let child = fixture.order(
        "child.toml",
        &order("child", "src/child.rs", "after = [\"one\"]"),
    );
    let (output, report) = fixture.run_json(&[one, two, child], &["--jobs", "2"]);
    assert!(output.status.success(), "{report}");
    let started = std::fs::read_to_string(barrier).unwrap();
    assert!(started.lines().any(|line| line == "one"), "{report}");
    assert!(started.lines().any(|line| line == "two"), "{report}");
}

#[test]
fn verifier_mutation_retains_the_worktree() {
    let fixture = Fixture::new("printf one > src/one.rs\ngit add .\ngit commit -qm work");
    fixture.write_config(&format!(
        "default_executor = \"fake\"\n[executors.fake]\nargv = [\"{}\"]\nprompt = \"stdin\"\ntimeout_secs = 10\n[verification.profiles.fast]\ncommands = [{{ argv = [\"sh\", \"-c\", \"printf bad >> src/one.rs; git add src/one.rs; git commit -qm verifier\"] }}]\n",
        fixture.executor.display()
    ));
    let path = fixture.order("mutate.toml", &order("mutate", "src/one.rs", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "verification_failed");
    let worktree = entry["worktree"].as_str().unwrap();
    assert!(std::path::Path::new(worktree).is_dir(), "{report}");
    assert!(entry["detail"].as_str().unwrap().contains("worktree kept"));
}

#[test]
fn detached_executor_state_is_retained_and_rejected() {
    let fixture = Fixture::new(
        "git checkout --detach\nprintf detached > src/detached.rs\ngit add .\ngit commit -qm detached",
    );
    let path = fixture.order("detached.toml", &order("detached", "src/detached.rs", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "error");
    assert!(entry["worktree"].as_str().is_some());
    assert!(entry["detail"].as_str().unwrap().contains("worktree kept"));
}

#[test]
fn executor_ref_move_is_retained_and_rejected() {
    let fixture = Fixture::new(
        "printf moved > src/moved.rs\ngit add .\ngit commit -qm moved\nref=$(git rev-parse --git-path refs/heads/$SUMMONER_BRANCH)\nprintf '%s\\n' \"$(git rev-parse HEAD~2)\" > \"$ref\"",
    );
    let path = fixture.order("ref-move.toml", &order("refmove", "src/moved.rs", ""));
    let (output, report) = fixture.run_json(&[path], &[]);
    assert_eq!(output.status.code(), Some(1), "{report}");
    let entry = &report["orders"][0];
    assert_eq!(entry["outcome"], "scope_violation");
    assert!(entry["detail"].as_str().unwrap().contains("worktree kept"));
    assert!(entry["worktree"].as_str().is_some());
}

#[test]
fn sigterm_reports_interruption_and_kills_the_active_group() {
    let fixture = Fixture::new("sleep 30");
    let path = fixture.order(
        "interrupt.toml",
        &order("interrupt", ".", "timeout_secs = 10"),
    );
    let child = Command::new(env!("CARGO_BIN_EXE_summoner"))
        .args(["run", path.to_str().unwrap()])
        .current_dir(&fixture.repo)
        .env("XDG_CONFIG_HOME", fixture.temp.path().join("config"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(400));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["orders"][0]["outcome"], "interrupted");
    assert!(report["orders"][0]["executor"]["interrupted"] == true);
}
