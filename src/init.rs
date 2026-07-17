//! `summoner init`: drop the orchestration contract into a repository so any
//! harness (Claude Code, Codex, OpenCode, anything driving a shell) learns the
//! same workflow from the repo itself. Same marker-gated, never-clobbering
//! pattern as `grove init`.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

const MARKER: &str = "<!-- summoner:agents:v1 -->";
const AGENTS_SECTION: &str = include_str!("../assets/agents-section.md");
const SKILL: &str = include_str!("../assets/skill.md");
const STARTER_TOML: &str = include_str!("../assets/summoner-starter.toml");

pub const CHARTER: &str = include_str!("../assets/charter.md");

#[derive(Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn init(workspace: &Path) -> Result<Report> {
    let mut written = Vec::new();
    let mut skipped = Vec::new();

    let toml = workspace.join(".summoner.toml");
    if toml.exists() {
        skipped.push(".summoner.toml".to_string());
    } else {
        std::fs::write(&toml, STARTER_TOML).context("writing .summoner.toml")?;
        written.push(".summoner.toml".to_string());
    }

    let agents = workspace.join("AGENTS.md");
    match std::fs::read_to_string(&agents) {
        Ok(existing) if existing.contains(MARKER) => skipped.push("AGENTS.md".to_string()),
        Ok(existing) => {
            let joined = format!("{}\n\n{}", existing.trim_end(), AGENTS_SECTION);
            std::fs::write(&agents, joined).context("appending to AGENTS.md")?;
            written.push("AGENTS.md (appended)".to_string());
        }
        Err(_) => {
            std::fs::write(&agents, format!("# Agent guide\n\n{AGENTS_SECTION}"))
                .context("writing AGENTS.md")?;
            written.push("AGENTS.md".to_string());
        }
    }

    let skill_dir = workspace.join(".claude").join("skills").join("summoner");
    let skill = skill_dir.join("SKILL.md");
    if skill.exists() {
        skipped.push(".claude/skills/summoner/SKILL.md".to_string());
    } else {
        std::fs::create_dir_all(&skill_dir).context("creating .claude/skills/summoner")?;
        std::fs::write(&skill, SKILL).context("writing SKILL.md")?;
        written.push(".claude/skills/summoner/SKILL.md".to_string());
    }

    Ok(Report { written, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_writes_everything_once_and_skips_everything_twice() {
        let dir = tempfile::tempdir().unwrap();

        let first = init(dir.path()).unwrap();
        assert_eq!(
            first.written,
            [
                ".summoner.toml",
                "AGENTS.md",
                ".claude/skills/summoner/SKILL.md"
            ]
        );
        assert!(first.skipped.is_empty());

        let second = init(dir.path()).unwrap();
        assert!(second.written.is_empty());
        assert_eq!(second.skipped.len(), 3);
    }

    #[test]
    fn existing_agents_md_is_appended_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# Existing rules\n\nKeep me.\n",
        )
        .unwrap();

        let report = init(dir.path()).unwrap();
        assert!(report.written.contains(&"AGENTS.md (appended)".to_string()));

        let merged = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(merged.starts_with("# Existing rules"));
        assert!(merged.contains("Keep me."));
        assert!(merged.contains(MARKER));
    }

    #[test]
    fn shipped_assets_are_internally_consistent() {
        // The marker gates idempotency, so the section must actually carry it.
        assert!(AGENTS_SECTION.contains(MARKER));
        // The starter file must parse as our own config and name real backends.
        let config: crate::config::Config = toml::from_str(STARTER_TOML).unwrap();
        assert_eq!(config.default_executor.as_deref(), Some("codex"));
        for name in ["codex", "glm", "claude"] {
            assert!(config.executors.contains_key(name), "missing preset {name}");
        }
        assert!(!CHARTER.trim().is_empty());
    }
}
