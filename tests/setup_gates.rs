//! Setup / onboarding integration tests.
#[path = "setup_support/mod.rs"]
mod support;
use support::*;

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

