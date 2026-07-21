use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

struct Fixture {
    _root: TempDir,
    repo: PathBuf,
    bin: PathBuf,
    config: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        let bin = root.path().join("bin");
        let config = root.path().join("config");
        let log = root.path().join("model.log");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "setup-test"]);
        git(&repo, &["config", "user.email", "setup@example.com"]);
        let fixture = Self {
            _root: root,
            repo,
            bin,
            config,
            log,
        };
        fixture.fake_tools();
        fixture
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(SUMMONER);
        command
            .args(args)
            .current_dir(&self.repo)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("APPDATA", &self.config)
            .env("FAKE_MODEL_LOG", &self.log)
            .env("PATH", prepend(&self.bin));
        for variable in [
            "CLAUDECODE",
            "CODEX_SANDBOX",
            "SUMMONER_PROFILE",
            "SUMMONER_DEFAULT_EXECUTOR",
            "SUMMONER_DEFAULT_REVIEWER",
        ] {
            command.env_remove(variable);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run summoner")
    }

    fn config_path(&self) -> PathBuf {
        self.config.join("summoner").join("config.toml")
    }

    fn fake_tools(&self) {
        fake(
            &self.bin,
            "grove",
            r#"if [ "$1" = "--version" ]; then echo 'grove 0.3.3'; exit 0; fi
if [ "$1" = "capabilities" ]; then echo '{"schema_version":1,"grove_version":"0.3.3","status":{"repository_schema":1,"task_status_schema":2,"task_record_schema":4},"inspection":{"binding_schema":1,"execution_schema":1,"process_tree":"unix_process_group_best_effort","filesystem":"read_only_permissions_and_digest","output":"captured_logs_json_report"}}'; exit 0; fi
if [ "$1" = "task" ]; then echo '{"schema_version":2,"tasks":[]}'; exit 0; fi
if [ "$1" = "plan" ]; then echo '{"sets":[],"conflicts":[],"couplings":[],"waves":[["summoner-demo"]]}'; exit 0; fi
exit 1"#,
            r#"if "%1"=="--version" (echo grove 0.3.3& exit /b 0)
if "%1"=="capabilities" (echo {"schema_version":1,"grove_version":"0.3.3","status":{"repository_schema":1,"task_status_schema":2,"task_record_schema":4},"inspection":{"binding_schema":1,"execution_schema":1,"process_tree":"windows_job_object","filesystem":"read_only_permissions_and_digest","output":"captured_logs_json_report"}}& exit /b 0)
if "%1"=="task" (echo {"schema_version":2,"tasks":[]}& exit /b 0)
if "%1"=="plan" (echo {"sets":[],"conflicts":[],"couplings":[],"waves":[["summoner-demo"]]}& exit /b 0)
exit /b 1"#,
        );
        for name in ["codex", "claude", "kimi"] {
            fake(
                &self.bin,
                name,
                r#"echo "$@" >> "$FAKE_MODEL_LOG"
exit 0"#,
                r#"echo %*>>"%FAKE_MODEL_LOG%"
exit /b 0"#,
            );
        }
    }
}

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
                "summoner run orders/example.toml"
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
        fixture.repo.join("AGENTS.md"),
        fixture.repo.join(".claude/skills/summoner/SKILL.md"),
        fixture.repo.join("orders/example.toml"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let output = fixture.run(&args);
    success(output.clone());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["written"], serde_json::json!([]));
    assert_eq!(report["skipped"].as_array().unwrap().len(), paths.len());
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
    for relative in [".summoner.toml", "AGENTS.md", "orders/example.toml"] {
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

#[test]
fn generated_example_checks_plans_and_reports_unverified_status_honestly() {
    let fixture = Fixture::new();
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    success(fixture.run(&["init", "--example"]));
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
    assert_eq!(report["orders"]["verification"][0]["profile"], Value::Null);
    assert!(report["notes"].as_array().unwrap().iter().any(|note| {
        note.as_str()
            .is_some_and(|note| note.contains("completed, not verified"))
    }));
}

#[test]
fn run_refuses_a_missing_executor_before_dispatch() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.config_path().parent().unwrap()).unwrap();
    std::fs::create_dir_all(fixture.repo.join("orders")).unwrap();
    std::fs::write(
        fixture.config_path(),
        "[executors.missing]\nargv = [\"definitely-not-installed-summoner-test-binary\", \"{prompt}\"]\n",
    )
    .unwrap();
    std::fs::write(
        fixture.repo.join("orders/missing.toml"),
        "id = \"missing\"\ntitle = \"Missing executor\"\nbrief = \"Write docs/missing.md and commit it.\"\nscope = [\"docs/missing.md\"]\nacceptance = [\"the note exists\"]\nexecutor = \"missing\"\n",
    )
    .unwrap();

    let output = fixture.run(&["run", "orders/missing.toml"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("preflight failed before dispatch"));
    assert!(stderr.contains("definitely-not-installed-summoner-test-binary"));
}

#[test]
fn run_refuses_missing_verification_profile_before_dispatch() {
    let fixture = Fixture::new();
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    std::fs::create_dir_all(fixture.repo.join("orders")).unwrap();
    std::fs::write(
        fixture.repo.join("orders/missing-profile.toml"),
        "id = \"profile\"\ntitle = \"Profile\"\nbrief = \"Do work.\"\nscope = [\"docs/work.md\"]\nacceptance = [\"done\"]\nverify_profile = \"missing\"\n",
    )
    .unwrap();
    let output = fixture.run(&["run", "orders/missing-profile.toml"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not defined"));
}

#[test]
fn run_refuses_missing_git_identity_before_dispatch() {
    let fixture = Fixture::new();
    success(fixture.run(&["init", "--global", "--preset", "codex"]));
    git(&fixture.repo, &["config", "--unset", "user.name"]);
    git(&fixture.repo, &["config", "--unset", "user.email"]);
    std::fs::create_dir_all(fixture.repo.join("orders")).unwrap();
    std::fs::write(
        fixture.repo.join("orders/identity.toml"),
        "id = \"identity\"\ntitle = \"Identity\"\nbrief = \"Do work.\"\nscope = [\"docs/work.md\"]\nacceptance = [\"done\"]\n",
    )
    .unwrap();
    let mut command = fixture.command(&["run", "orders/identity.toml"]);
    command.env(
        "GIT_CONFIG_GLOBAL",
        fixture.repo.join("no-global-gitconfig"),
    );
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("git_identity"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    );
}

fn prepend(bin: &Path) -> OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&existing)))
        .unwrap()
}

#[cfg(unix)]
fn fake(dir: &Path, name: &str, unix: &str, _windows: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{unix}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(windows)]
fn fake(dir: &Path, name: &str, _unix: &str, windows: &str) {
    std::fs::write(
        dir.join(format!("{name}.CMD")),
        format!("@echo off\r\n{windows}\r\n"),
    )
    .unwrap();
}
