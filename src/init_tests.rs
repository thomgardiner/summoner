use super::*;

#[test]
fn failed_atomic_rollback_preserves_written_config_and_grove_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let written = b"generated config".to_vec();
    std::fs::write(&path, &written).unwrap();
    let snapshot = GlobalSnapshot {
        path: path.clone(),
        original: Some(b"original config".to_vec()),
        written: Some(written.clone()),
    };

    let error = snapshot.rollback_with(
        anyhow::anyhow!("grove lock generation failed"),
        |path, contents| {
            global::write_atomic_with(path, contents, |_temporary, _destination| {
                Err(std::io::Error::other("injected rollback replace failure"))
            })
        },
    );

    let chain = format!("{error:#}");
    assert!(
        chain.contains("injected rollback replace failure"),
        "{chain}"
    );
    assert!(chain.contains("grove lock generation failed"), "{chain}");
    assert_eq!(std::fs::read(path).unwrap(), written);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn init_writes_once_and_skips_twice() {
    let dir = tempfile::tempdir().unwrap();
    let first = init(dir.path(), false).unwrap();
    // No Claude Code in evidence, so no vendor furniture: the skill file is
    // skipped, and AGENTS.md carries the whole contract.
    assert_eq!(first.written, [".summoner.toml", "AGENTS.md"]);
    assert_eq!(first.skipped.len(), 1, "{:?}", first.skipped);
    assert!(
        first.skipped[0].contains("no Claude Code"),
        "{:?}",
        first.skipped
    );
    let second = init(dir.path(), false).unwrap();
    assert!(second.written.is_empty());
    assert_eq!(second.skipped.len(), 3);
}

/// The skill is Claude Code furniture: written only where that harness is
/// already in evidence, never dropped into repositories driven by others.
#[test]
fn the_claude_skill_is_written_only_where_claude_is_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let report = init(dir.path(), false).unwrap();
    assert!(
        report
            .written
            .iter()
            .any(|entry| entry.contains("SKILL.md")),
        "{report:?}"
    );

    let bare = tempfile::tempdir().unwrap();
    let report = init(bare.path(), false).unwrap();
    assert!(
        !report
            .written
            .iter()
            .any(|entry| entry.contains("SKILL.md")),
        "{report:?}"
    );
    assert!(!bare.path().join(".claude").exists());
}

#[test]
fn existing_agents_md_is_appended_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Existing\n\nKeep me.\n").unwrap();
    init(dir.path(), false).unwrap();
    let merged = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(merged.starts_with("# Existing"));
    assert!(merged.contains("Keep me."));
    assert!(merged.contains(MARKER));
}

#[test]
fn refresh_updates_managed_content_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        format!("# Mine\n\nKeep me.\n\n{MARKER}\nold\n{END_MARKER}\n\nAfter.\n"),
    )
    .unwrap();
    let skill_dir = dir.path().join(".claude/skills/summoner");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "old skill\n").unwrap();
    std::fs::write(dir.path().join(".summoner.toml"), "max_parallel = 7\n").unwrap();
    let report = init(dir.path(), true).unwrap();
    assert!(
        report
            .written
            .contains(&"AGENTS.md (refreshed)".to_string())
    );
    assert!(
        report
            .written
            .contains(&".claude/skills/summoner/SKILL.md (refreshed)".to_string())
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".summoner.toml")).unwrap(),
        "max_parallel = 7\n"
    );
    let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("Keep me."));
    assert!(agents.contains("After."));
    assert!(!agents.contains("\nold\n"));
}

#[test]
fn example_is_safe_parseable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(dir.path().join("Cargo.lock"), "version = 4\n").unwrap();
    let grove = crate::grove::GroveCli::new("unused-grove".into());
    let first = example(dir.path(), false, &grove).unwrap();
    assert!(first.written.contains(&"orders/example.toml".to_string()));
    let path = dir.path().join("orders/example.toml");
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["id"].as_str(), Some("summoner-demo"));
    assert!(
        value["acceptance"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert_eq!(value["verify_profile"].as_str(), Some("rust-check"));
    assert!(dir.path().join(".grove.toml").is_file());
    let before = std::fs::read(&path).unwrap();
    let second = example(dir.path(), false, &grove).unwrap();
    assert!(second.skipped.contains(&"orders/example.toml".to_string()));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn example_selects_a_real_required_grove_profile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".grove.toml"),
        "[verification]\nrequired = [\"fast\"]\n[verification.profiles.fast]\ncommands = [{ argv = [\"true\"] }]\n",
    )
    .unwrap();
    example(
        dir.path(),
        false,
        &crate::grove::GroveCli::new("unused-grove".into()),
    )
    .unwrap();
    let value: toml::Value =
        toml::from_str(&std::fs::read_to_string(dir.path().join("orders/example.toml")).unwrap())
            .unwrap();
    assert_eq!(value["verify_profile"].as_str(), Some("fast"));
}

#[test]
fn example_refuses_ambiguous_user_owned_verification() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".grove.toml"),
        "[verification.profiles.a]\ncommands = [{ argv = [\"true\"] }]\n[verification.profiles.b]\ncommands = [{ argv = [\"true\"] }]\n",
    )
    .unwrap();
    let error = example(
        dir.path(),
        false,
        &crate::grove::GroveCli::new("unused-grove".into()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("no single required usable"));
    assert!(!dir.path().join(".summoner.toml").exists());
}

#[test]
fn example_refuses_a_single_nonrequired_profile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".grove.toml"),
        "[verification.profiles.fast]\ncommands = [{ argv = [\"true\"] }]\n",
    )
    .unwrap();
    let error = example(
        dir.path(),
        false,
        &crate::grove::GroveCli::new("unused-grove".into()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("no single required usable"));
    assert!(!dir.path().join(".summoner.toml").exists());
}

#[test]
fn shipped_assets_are_internally_consistent() {
    assert!(AGENTS_SECTION.contains(MARKER));
    let config: crate::config::Config = toml::from_str(STARTER_TOML).unwrap();
    assert!(config.executors.is_empty());
    assert!(config.default_executor.is_none());
    for token in [
        "{prompt}",
        "{prompt_file}",
        "{worktree}",
        "{git_common_dir}",
        "{order_file}",
        "\"stdin\"",
        "usage_marker",
        "env_required",
        "init --global",
    ] {
        assert!(STARTER_TOML.contains(token), "template lost {token}");
    }
    assert!(!CHARTER.trim().is_empty());
}
