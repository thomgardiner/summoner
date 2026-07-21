use super::*;

#[test]
fn init_writes_once_and_skips_twice() {
    let dir = tempfile::tempdir().unwrap();
    let first = init(dir.path(), false).unwrap();
    assert_eq!(
        first.written,
        [
            ".summoner.toml",
            "AGENTS.md",
            ".claude/skills/summoner/SKILL.md"
        ]
    );
    assert!(first.skipped.is_empty());
    let second = init(dir.path(), false).unwrap();
    assert!(second.written.is_empty());
    assert_eq!(second.skipped.len(), 3);
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
    let first = example(dir.path(), false).unwrap();
    assert!(first.written.contains(&"orders/example.toml".to_string()));
    let path = dir.path().join("orders/example.toml");
    let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["id"].as_str(), Some("summoner-demo"));
    assert!(
        value["acceptance"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(value.get("verify_profile").is_none());
    let before = std::fs::read(&path).unwrap();
    let second = example(dir.path(), false).unwrap();
    assert!(second.skipped.contains(&"orders/example.toml".to_string()));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn example_selects_a_real_unambiguous_grove_profile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".grove.toml"),
        "[verification]\nrequired = [\"fast\"]\n[verification.profiles.fast]\ncommands = [{ argv = [\"true\"] }]\n",
    )
    .unwrap();
    example(dir.path(), false).unwrap();
    let value: toml::Value =
        toml::from_str(&std::fs::read_to_string(dir.path().join("orders/example.toml")).unwrap())
            .unwrap();
    assert_eq!(value["verify_profile"].as_str(), Some("fast"));
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
