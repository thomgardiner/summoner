//! Setup / onboarding integration tests.
#[path = "setup_support/mod.rs"]
mod support;
use serde_json::Value;
use support::*;

#[test]
fn doctor_runs_fake_bounded_diagnostics_for_the_selected_lifecycle() {
    let fixture = Fixture::new();
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    let output = fixture.run(&["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["executors"].as_array().unwrap().len(), 2);
    assert!(
        report["executors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|executor| executor["diagnostic"]["auth"] == "passed")
    );
    let log = std::fs::read_to_string(&fixture.log).unwrap();
    assert_eq!(
        log.lines().filter(|line| *line == "login status").count(),
        2
    );
}

#[test]
fn unknown_auth_requires_persisted_or_cli_acknowledgement() {
    let fixture = Fixture::new();
    success(fixture.run(&["init", "--global", "--preset", "kimi"]));
    let path = fixture.config_path();
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("allow_unknown_auth"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, text).unwrap();

    let denied = fixture.run(&["doctor"]);
    assert_eq!(denied.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&denied.stdout).unwrap();
    assert!(report["next_steps"].as_array().unwrap().iter().any(|step| {
        step.as_str()
            .is_some_and(|step| step.contains("--allow-unknown-auth"))
    }));

    success(fixture.run(&["--allow-unknown-auth", "doctor"]));
}

#[test]
fn malformed_existing_config_is_a_usage_error() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    std::fs::write(fixture.config_path(), "max_paralel = 4\n").unwrap();
    let output = fixture.run(&["config"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("parsing"));
}

#[test]
fn malformed_config_is_rejected_before_example_touches_the_repo() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    std::fs::write(fixture.config_path(), "max_paralel = 4\n").unwrap();
    std::fs::write(fixture.repo.join("AGENTS.md"), "# Existing\n\nKeep me.\n").unwrap();
    let before = git_output(&fixture.repo, &["status", "--porcelain"]);

    let output = fixture.run(&["init", "--example"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("parsing"));
    assert_eq!(
        git_output(&fixture.repo, &["status", "--porcelain"]),
        before
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("AGENTS.md")).unwrap(),
        "# Existing\n\nKeep me.\n"
    );
    for path in [
        ".summoner.toml",
        ".grove.toml",
        ".claude/skills/summoner/SKILL.md",
        "orders/example.toml",
        "Cargo.lock",
    ] {
        assert!(!fixture.repo.join(path).exists(), "created {path}");
    }
}

#[test]
fn doctor_without_default_is_actionable_and_non_green() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.config_path(),
        "[executors.fake]\nargv = [\"fake\", \"{prompt}\"]\n",
    )
    .unwrap();
    let output = fixture.run(&["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert!(report["next_steps"].as_array().unwrap().iter().any(|step| {
        step.as_str()
            .is_some_and(|step| step.contains("init --global --preset codex"))
    }));
}
