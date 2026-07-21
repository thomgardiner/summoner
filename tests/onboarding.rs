use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

#[test]
fn generated_demo_profile_runs_locked_without_first_run_source_drift() {
    let root = TempDir::new().expect("create fixture root");
    let repo = root.path().join("fresh demo with space");
    assert_success(
        Command::new("cargo")
            .args(["new", "--bin", "--vcs", "git", "--name", "fresh-demo"])
            .arg(&repo)
            .output()
            .expect("create a fresh Rust binary repository"),
    );
    assert!(!repo.join("Cargo.lock").exists());

    let grove = std::env::var("SUMMONER_TEST_GROVE").unwrap_or_else(|_| "grove".into());
    let config = root.path().join("config");
    let cache = root.path().join("grove-cache");
    assert_success(
        Command::new(SUMMONER)
            .args(["init", "--example"])
            .current_dir(&repo)
            .env("SUMMONER_GROVE_BIN", &grove)
            .env("GROVE_CACHE_ROOT", &cache)
            .env("XDG_CONFIG_HOME", &config)
            .env("APPDATA", &config)
            .output()
            .expect("initialize the generated demo"),
    );
    assert!(repo.join("Cargo.lock").is_file());

    let profile: toml::Value = toml::from_str(
        &std::fs::read_to_string(repo.join(".grove.toml")).expect("read generated Grove config"),
    )
    .expect("parse generated Grove config");
    let argv = profile["verification"]["profiles"]["rust-check"]["commands"][0]["argv"]
        .as_array()
        .expect("generated rust-check argv")
        .iter()
        .map(|arg| arg.as_str().expect("string argv element"))
        .collect::<Vec<_>>();
    assert!(argv.contains(&"--locked"));

    let before = git_output(&repo, &["status", "--porcelain"]);
    assert_success(
        Command::new(&grove)
            .args(["exec", "--tag", "generated-demo-profile", "--"])
            .args(argv)
            .current_dir(&repo)
            .env("GROVE_CACHE_ROOT", &cache)
            .output()
            .expect("execute generated verification through Grove"),
    );
    assert_eq!(git_output(&repo, &["status", "--porcelain"]), before);
}

fn git_output(repo: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("run git");
    assert_success(output.clone());
    output.stdout
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
