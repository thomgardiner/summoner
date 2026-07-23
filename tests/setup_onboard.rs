//! Setup / onboarding integration tests.
#[path = "setup_support/mod.rs"]
mod support;
use support::*;
use serde_json::Value;

#[test]
fn one_command_onboards_each_explicit_preset_and_prints_next_steps() {
    for preset in ["codex", "claude", "kimi"] {
        let fixture = Fixture::new();
        let output = fixture.run(&["init", "--preset", preset, "--example"]);
        success(output.clone());
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["next_steps"],
            serde_json::json!([
                "summoner doctor orders/example.toml",
                "summoner plan orders/example.toml",
                "summoner run --stream orders/example.toml"
            ])
        );
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(fixture.config_path()).unwrap()).unwrap();
        assert_eq!(config["default_executor"].as_str(), Some(preset));
        assert_eq!(
            config["default_reviewer"].as_str(),
            Some(format!("{preset}-review").as_str())
        );
        assert!(fixture.repo.join(".summoner.toml").is_file());
        assert!(fixture.repo.join(".grove.toml").is_file());
        assert!(fixture.repo.join("AGENTS.md").is_file());
        assert!(fixture.repo.join("orders/example.toml").is_file());
        if preset == "kimi" {
            assert_eq!(
                config["allow_unknown_auth"],
                toml::Value::Array(vec!["kimi".into(), "kimi-review".into()])
            );
        }
    }
}


#[test]
fn one_command_onboarding_is_idempotent() {
    let fixture = Fixture::new();
    let args = ["init", "--preset", "codex", "--example"];
    success(fixture.run(&args));
    let paths = [
        fixture.config_path(),
        fixture.repo.join(".summoner.toml"),
        fixture.repo.join(".grove.toml"),
        fixture.repo.join("AGENTS.md"),
        fixture.repo.join("orders/example.toml"),
    ];
    // A codex onboarding leaves no Claude furniture behind: the skill file is
    // written only where Claude Code is already in evidence.
    assert!(
        !fixture.repo.join(".claude").exists(),
        "no .claude/ residue for a codex preset"
    );
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let output = fixture.run(&args);
    success(output.clone());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["written"], serde_json::json!([]));
    // Every managed path skips, plus the skill notice explaining its absence.
    assert_eq!(report["skipped"].as_array().unwrap().len(), paths.len() + 1);
    for (path, expected) in paths.iter().zip(before) {
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
}


#[test]
fn one_command_refuses_a_preset_conflict_before_touching_the_repo() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    let existing = "[executors.codex]\nargv = [\"mine\"]\n";
    std::fs::write(fixture.config_path(), existing).unwrap();
    let output = fixture.run(&["init", "--preset", "codex", "--example"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("refusing to replace"));
    assert_eq!(
        std::fs::read_to_string(fixture.config_path()).unwrap(),
        existing
    );
    for relative in [
        ".summoner.toml",
        ".grove.toml",
        "AGENTS.md",
        "orders/example.toml",
    ] {
        assert!(!fixture.repo.join(relative).exists(), "created {relative}");
    }
}


#[test]
fn presets_append_preserve_and_remain_idempotent_through_the_cli() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    std::fs::write(
        fixture.config_path(),
        "# keep this comment\nmax_parallel = 3\n",
    )
    .unwrap();
    for preset in ["codex", "claude", "kimi"] {
        success(fixture.run(&["init", "--global", "--preset", preset]));
    }
    let before = std::fs::read(fixture.config_path()).unwrap();
    let text = String::from_utf8(before.clone()).unwrap();
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert!(text.contains("# keep this comment"));
    assert_eq!(config["default_executor"].as_str(), Some("codex"));
    assert_eq!(config["default_reviewer"].as_str(), Some("codex-review"));
    for name in [
        "codex",
        "codex-review",
        "claude",
        "claude-review",
        "kimi",
        "kimi-review",
    ] {
        assert!(config["executors"].get(name).is_some(), "missing {name}");
    }
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);

    std::fs::create_dir_all(fixture.repo.join("orders")).unwrap();
    std::fs::write(
        fixture.repo.join("orders/exact.toml"),
        "id = \"exact\"\ntitle = \"Exact roles\"\nbrief = \"Write docs/exact.md and commit it.\"\nscope = [\"docs/exact.md\"]\nacceptance = [\"the note exists\"]\nexecutor = \"codex\"\nreviewer = \"codex-review\"\n",
    )
    .unwrap();
    let output = fixture.run(&["doctor", "orders/exact.toml"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<_> = report["executors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|executor| executor["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["codex", "codex-review"]);
}


#[test]
fn generated_example_checks_plans_and_selects_real_rust_verification() {
    let fixture = Fixture::new();
    assert!(!fixture.repo.join("Cargo.lock").exists());
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    let initialized = fixture.run(&["init", "--example"]);
    success(initialized.clone());
    let init_report: Value = serde_json::from_slice(&initialized.stdout).unwrap();
    assert!(
        init_report["written"]
            .as_array()
            .unwrap()
            .contains(&"Cargo.lock".into())
    );
    assert!(fixture.repo.join("Cargo.lock").is_file());
    success(fixture.run(&["check", "orders/example.toml"]));
    success(fixture.run(&["plan", "orders/example.toml"]));
    let output = fixture.run(&["doctor", "orders/example.toml"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["orders"]["roles"],
        serde_json::json!(["codex", "codex-review"])
    );
    assert_eq!(report["orders"]["verification"][0]["profile"], "rust-check");
    assert_eq!(report["orders"]["verification"][0]["configured"], true);
    assert_eq!(report["ok"], true);
    // Notes may list harness skills / host notices; they must not fail the bar.
}


#[test]
fn failed_lock_generation_rolls_back_owned_profile_and_retry_succeeds() {
    let fixture = Fixture::new();
    std::fs::write(fixture.repo.join("AGENTS.md"), "# Existing\n\nKeep me.\n").unwrap();
    let touched = [
        ".summoner.toml",
        ".grove.toml",
        "AGENTS.md",
        ".claude/skills/summoner/SKILL.md",
        "orders/example.toml",
        "Cargo.lock",
    ];
    let before = touched
        .iter()
        .map(|path| std::fs::read(fixture.repo.join(path)).ok())
        .collect::<Vec<_>>();
    let mut first = fixture.command(&["init", "--example"]);
    first.env("FAKE_GROVE_FAIL_EXEC", "1");
    let failed = first.output().unwrap();
    assert_eq!(failed.status.code(), Some(2));
    for (path, expected) in touched.iter().zip(&before) {
        assert_eq!(
            &std::fs::read(fixture.repo.join(path)).ok(),
            expected,
            "{path}"
        );
    }
    assert!(!fixture.repo.join(".claude").exists());
    assert!(!fixture.repo.join("orders").exists());

    success(fixture.run(&["init", "--example"]));
    assert!(fixture.repo.join(".grove.toml").is_file());
    assert!(fixture.repo.join("Cargo.lock").is_file());
}


#[test]
fn example_does_not_generate_a_lock_for_user_owned_grove_config() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.repo.join(".grove.toml"),
        "[verification]\nrequired = [\"custom\"]\n[verification.profiles.custom]\ncommands = [{ argv = [\"true\"] }]\n",
    )
    .unwrap();
    success(fixture.run(&["init", "--example"]));
    assert!(!fixture.repo.join("Cargo.lock").exists());
}


