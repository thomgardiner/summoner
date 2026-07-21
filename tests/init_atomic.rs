use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

struct Fixture {
    _root: TempDir,
    repo: PathBuf,
    config: PathBuf,
    grove: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("fixture root");
        let repo = root.path().join("repo");
        let config = root.path().join("config");
        std::fs::create_dir_all(&repo).expect("repo directory");
        std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").expect("manifest");
        std::fs::write(repo.join("AGENTS.md"), "# Existing\n\nKeep me.\n").expect("agents");
        let grove = fake_grove(root.path());
        Self {
            _root: root,
            repo,
            config,
            grove,
        }
    }

    fn global(&self) -> PathBuf {
        self.config.join("summoner").join("config.toml")
    }

    fn write_global(&self, text: &str) {
        std::fs::create_dir_all(self.global().parent().expect("global config parent"))
            .expect("config directory");
        std::fs::write(self.global(), text).expect("global config");
    }

    fn run(&self, fail_grove: bool, replace_global: bool) -> Output {
        let mut command = Command::new(SUMMONER);
        command
            .args(["init", "--preset", "codex", "--example"])
            .current_dir(&self.repo)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("APPDATA", &self.config)
            .env("SUMMONER_GROVE_BIN", &self.grove)
            .env("FAKE_GLOBAL_CONFIG", self.global());
        if fail_grove {
            command.env("FAKE_GROVE_FAIL_EXEC", "1");
        }
        if replace_global {
            command.env("FAKE_GROVE_REPLACE_GLOBAL", "1");
        }
        command.output().expect("run Summoner")
    }
}

#[test]
fn malformed_repo_config_leaves_combined_onboarding_unchanged_and_retry_succeeds() {
    let fixture = Fixture::new();
    let global = "# keep global\nmax_parallel = 3\n";
    fixture.write_global(global);
    std::fs::write(fixture.repo.join(".summoner.toml"), "max_paralel = 4\n").unwrap();
    let before = repo_state(&fixture.repo);

    let failed = fixture.run(false, false);
    assert_eq!(failed.status.code(), Some(2));
    assert_eq!(std::fs::read(fixture.global()).unwrap(), global.as_bytes());
    assert_eq!(repo_state(&fixture.repo), before);

    std::fs::write(fixture.repo.join(".summoner.toml"), "max_parallel = 4\n").unwrap();
    assert_success(fixture.run(false, false));
    assert!(fixture.repo.join("Cargo.lock").is_file());
    assert!(fixture.repo.join("orders/example.toml").is_file());
}

#[test]
fn grove_failure_restores_combined_onboarding_and_retry_succeeds() {
    let fixture = Fixture::new();
    let global = "# keep global\nmax_parallel = 3\n";
    fixture.write_global(global);
    let before = repo_state(&fixture.repo);

    let failed = fixture.run(true, false);
    assert_eq!(failed.status.code(), Some(2));
    assert_eq!(std::fs::read(fixture.global()).unwrap(), global.as_bytes());
    assert_eq!(repo_state(&fixture.repo), before);

    assert_success(fixture.run(false, false));
    assert!(fixture.repo.join("Cargo.lock").is_file());
    assert!(fixture.repo.join("orders/example.toml").is_file());
}

#[test]
fn unchanged_generated_global_is_removed_after_failure() {
    let fixture = Fixture::new();
    let before = repo_state(&fixture.repo);

    let failed = fixture.run(true, false);
    assert_eq!(failed.status.code(), Some(2));
    assert!(!fixture.global().exists());
    assert_eq!(repo_state(&fixture.repo), before);

    assert_success(fixture.run(false, false));
    assert!(fixture.global().is_file());
}

#[test]
fn concurrent_global_replacement_is_preserved_with_original_error_context() {
    let fixture = Fixture::new();
    fixture.write_global("# original\nmax_parallel = 3\n");

    let failed = fixture.run(true, true);
    assert_concurrent_error(&failed);
    assert_eq!(
        std::fs::read_to_string(fixture.global()).unwrap(),
        "# concurrent\nmax_parallel = 7\n"
    );
}

#[test]
fn concurrent_replacement_of_generated_global_is_preserved() {
    let fixture = Fixture::new();
    assert!(!fixture.global().exists());

    let failed = fixture.run(true, true);
    assert_concurrent_error(&failed);
    assert_eq!(
        std::fs::read_to_string(fixture.global()).unwrap(),
        "# concurrent\nmax_parallel = 7\n"
    );
}

fn assert_concurrent_error(output: &Output) {
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("grove could not generate Cargo.lock"),
        "{error}"
    );
    assert!(error.contains("changed concurrently"), "{error}");
    assert!(error.contains("reconcile"), "{error}");
}

fn repo_state(repo: &Path) -> Vec<Option<Vec<u8>>> {
    [
        "Cargo.toml",
        "Cargo.lock",
        ".summoner.toml",
        ".grove.toml",
        "AGENTS.md",
        ".claude/skills/summoner/SKILL.md",
        "orders/example.toml",
    ]
    .iter()
    .map(|path| std::fs::read(repo.join(path)).ok())
    .collect()
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn fake_grove(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("grove");
    std::fs::write(
        &path,
        "#!/bin/sh\nif [ \"$1\" = exec ] && [ \"$FAKE_GROVE_REPLACE_GLOBAL\" = 1 ]; then printf '# concurrent\\nmax_parallel = 7\\n' > \"$FAKE_GLOBAL_CONFIG\"; fi\nif [ \"$1\" = exec ] && [ \"$FAKE_GROVE_FAIL_EXEC\" = 1 ]; then printf 'partial\\n' > Cargo.lock; exit 1; fi\nif [ \"$1\" = exec ]; then printf 'version = 4\\n' > Cargo.lock; exit 0; fi\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(windows)]
fn fake_grove(root: &Path) -> PathBuf {
    let path = root.join("grove.cmd");
    std::fs::write(
        &path,
        "@echo off\r\nif \"%1\"==\"exec\" if \"%FAKE_GROVE_REPLACE_GLOBAL%\"==\"1\" ((echo # concurrent& echo max_parallel = 7)>\"%FAKE_GLOBAL_CONFIG%\")\r\nif \"%1\"==\"exec\" if \"%FAKE_GROVE_FAIL_EXEC%\"==\"1\" ((echo partial)>Cargo.lock& exit /b 1)\r\nif \"%1\"==\"exec\" ((echo version = 4)>Cargo.lock& exit /b 0)\r\nexit /b 1\r\n",
    )
    .unwrap();
    path
}
