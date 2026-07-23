//! Install Summoner as a user-level skill in agent harness skill directories.
//!
//! Harnesses discover skills from well-known homes (Claude Code, Codex,
//! Agents/Grok). Repo-local `.claude/skills` stays optional via `init`; this
//! module is the ergonomics path so `/summoner` works without per-repo setup.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

const SKILL: &str = include_str!("../assets/skill.md");
const MARKER: &str = "<!-- summoner:skill:v1 -->";
const SKILL_NAME: &str = "summoner";

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
    pub next_steps: Vec<String>,
}

/// Install or refresh the Summoner skill into every supported user skill root.
pub fn install_user_skills(refresh: bool) -> Result<Report> {
    let home = home_dir().context("HOME/USERPROFILE is unset; cannot install user skills")?;
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    install_into(&home, codex_home.as_deref(), refresh)
}

/// Skill directories that already contain a managed Summoner skill.
pub fn installed_paths() -> Vec<PathBuf> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);
    skill_roots(&home, codex_home.as_deref())
        .into_iter()
        .map(|root| root.join(SKILL_NAME).join("SKILL.md"))
        .filter(|path| {
            std::fs::read_to_string(path)
                .map(|text| text.contains(MARKER))
                .unwrap_or(false)
        })
        .collect()
}

pub(crate) fn install_into(
    home: &Path,
    codex_home: Option<&Path>,
    refresh: bool,
) -> Result<Report> {
    let mut report = Report::default();
    for root in skill_roots(home, codex_home) {
        install_one(&root, refresh, &mut report)?;
    }
    if report.written.is_empty() && report.skipped.is_empty() {
        report
            .skipped
            .push("no skill roots resolved (unexpected)".into());
    }
    report.next_steps = next_steps(&report);
    Ok(report)
}

fn skill_roots(home: &Path, codex_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.push(home.join(".claude").join("skills"));
    let codex = codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"));
    roots.push(codex.join("skills"));
    roots.push(home.join(".agents").join("skills"));
    roots.push(home.join(".grok").join("skills"));
    roots
}

fn install_one(root: &Path, refresh: bool, report: &mut Report) -> Result<()> {
    let dir = root.join(SKILL_NAME);
    let path = dir.join("SKILL.md");
    let label = path.display().to_string();
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == SKILL => {
            report.skipped.push(label);
            return Ok(());
        }
        Ok(existing) if existing.contains(MARKER) && !refresh => {
            report
                .skipped
                .push(format!("{label} (managed; pass --refresh to update)"));
            return Ok(());
        }
        Ok(existing) if !existing.contains(MARKER) && !refresh => {
            report.skipped.push(format!(
                "{label} (exists, not summoner-managed; pass --refresh to overwrite)"
            ));
            return Ok(());
        }
        Ok(_) | Err(_) => {}
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating skill directory {}", dir.display()))?;
    std::fs::write(&path, SKILL).with_context(|| format!("writing {label}"))?;
    report.written.push(label);
    Ok(())
}

fn next_steps(report: &Report) -> Vec<String> {
    let mut steps = vec![
        "Claude Code: type /summoner (or ask to run a Summoner fleet)".into(),
        "Codex: ask to plan/run orders — skill is under ~/.codex/skills/summoner".into(),
        "Shell: summoner init --example && summoner doctor orders/ && summoner plan orders/".into(),
    ];
    if report.written.iter().any(|p| p.contains(".claude"))
        || report
            .skipped
            .iter()
            .any(|p| p.contains(".claude") && !p.contains("not summoner"))
    {
        steps.insert(
            0,
            "Reload Claude Code (or start a new session) so /summoner is listed".into(),
        );
    }
    steps.push(
        "Optional host depth for Rust monorepos: install grove, then [host] kind = \"grove\""
            .into(),
    );
    steps
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_into_all_harness_roots_under_temp_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let report = install_into(home, None, false).unwrap();
        assert!(
            report.written.len() >= 4,
            "expected multi-harness install, got {report:?}"
        );
        for root in skill_roots(home, None) {
            let path = root.join("summoner").join("SKILL.md");
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{}", path.display()));
            assert!(text.contains(MARKER), "{path:?}");
            assert!(text.contains("/summoner"), "{path:?}");
        }
        let again = install_into(home, None, false).unwrap();
        assert!(again.written.is_empty(), "{again:?}");
        assert!(!again.skipped.is_empty());
    }

    #[test]
    fn respects_codex_home_override() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let codex = tmp.path().join("custom-codex");
        std::fs::create_dir_all(&home).unwrap();
        let report = install_into(&home, Some(&codex), false).unwrap();
        assert!(
            report
                .written
                .iter()
                .any(|p| p.contains("custom-codex") && p.contains("summoner")),
            "{report:?}"
        );
    }
}
