//! Idempotent repository initialization.

#[path = "init_global.rs"]
mod global;
#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;

use anyhow::{Context, Result};
pub use global::global;
use serde::Serialize;
use std::path::Path;

const MARKER: &str = "<!-- summoner:agents:v1 -->";
const END_MARKER: &str = "<!-- summoner:agents:end -->";
const AGENTS_SECTION: &str = include_str!("../assets/agents-section.md");
const SKILL: &str = include_str!("../assets/skill.md");
const STARTER_TOML: &str = include_str!("../assets/summoner-starter.toml");
const EXAMPLE_ORDER: &str = include_str!("../assets/example-order.toml");

pub const CHARTER: &str = include_str!("../assets/charter.md");
pub const REVIEW_CHARTER: &str = include_str!("../assets/review-charter.md");

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub written: Vec<String>,
    pub skipped: Vec<String>,
}

impl Report {
    pub fn merge(&mut self, other: Self) {
        self.written.extend(other.written);
        self.skipped.extend(other.skipped);
    }
}

pub fn init(workspace: &Path, refresh: bool) -> Result<Report> {
    let mut report = Report::default();
    write_repo_config(workspace, &mut report)?;
    write_agents(workspace, refresh, &mut report)?;
    write_skill(workspace, refresh, &mut report)?;
    Ok(report)
}

pub fn example(workspace: &Path, refresh: bool) -> Result<Report> {
    let mut report = init(workspace, refresh)?;
    let path = workspace.join("orders").join("example.toml");
    if path.exists() {
        report.skipped.push("orders/example.toml".to_string());
    } else {
        std::fs::create_dir_all(path.parent().context("example order has no parent")?)
            .context("creating orders directory")?;
        std::fs::write(&path, example_order(workspace)?).context("writing orders/example.toml")?;
        report.written.push("orders/example.toml".to_string());
    }
    Ok(report)
}

fn example_order(workspace: &Path) -> Result<String> {
    let Some(profile) = crate::config::grove_profiles(workspace)?.selected else {
        return Ok(EXAMPLE_ORDER.to_string());
    };
    Ok(format!("{EXAMPLE_ORDER}verify_profile = {profile:?}\n"))
}

fn write_repo_config(workspace: &Path, report: &mut Report) -> Result<()> {
    let path = workspace.join(".summoner.toml");
    if path.exists() {
        report.skipped.push(".summoner.toml".to_string());
    } else {
        std::fs::write(&path, STARTER_TOML).context("writing .summoner.toml")?;
        report.written.push(".summoner.toml".to_string());
    }
    Ok(())
}

fn write_agents(workspace: &Path, refresh: bool, report: &mut Report) -> Result<()> {
    let path = workspace.join("AGENTS.md");
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing.contains(MARKER) => {
            match replace_section(&existing).filter(|_| refresh) {
                Some(replaced) if replaced != existing => {
                    std::fs::write(&path, replaced).context("refreshing AGENTS.md")?;
                    report.written.push("AGENTS.md (refreshed)".to_string());
                }
                _ => report.skipped.push("AGENTS.md".to_string()),
            }
        }
        Ok(existing) => {
            let joined = format!("{}\n\n{}", existing.trim_end(), AGENTS_SECTION);
            std::fs::write(&path, joined).context("appending to AGENTS.md")?;
            report.written.push("AGENTS.md (appended)".to_string());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&path, format!("# Agent guide\n\n{AGENTS_SECTION}"))
                .context("writing AGENTS.md")?;
            report.written.push("AGENTS.md".to_string());
        }
        Err(error) => return Err(error).context("reading AGENTS.md"),
    }
    Ok(())
}

fn write_skill(workspace: &Path, refresh: bool, report: &mut Report) -> Result<()> {
    let dir = workspace.join(".claude").join("skills").join("summoner");
    let path = dir.join("SKILL.md");
    let stale = refresh
        && std::fs::read_to_string(&path)
            .map(|existing| existing != SKILL)
            .unwrap_or(false);
    if path.exists() && !stale {
        report
            .skipped
            .push(".claude/skills/summoner/SKILL.md".to_string());
        return Ok(());
    }
    std::fs::create_dir_all(&dir).context("creating .claude/skills/summoner")?;
    std::fs::write(&path, SKILL).context("writing SKILL.md")?;
    let label = if stale {
        ".claude/skills/summoner/SKILL.md (refreshed)"
    } else {
        ".claude/skills/summoner/SKILL.md"
    };
    report.written.push(label.to_string());
    Ok(())
}

fn replace_section(existing: &str) -> Option<String> {
    let start = existing.find(MARKER)?;
    let end = existing[start..]
        .find(END_MARKER)
        .map(|at| start + at + END_MARKER.len())
        .unwrap_or(existing.len());
    let mut replaced = String::with_capacity(existing.len() + AGENTS_SECTION.len());
    replaced.push_str(&existing[..start]);
    replaced.push_str(AGENTS_SECTION.trim_end());
    replaced.push_str(existing[end..].trim_end_matches('\n'));
    replaced.push('\n');
    Some(replaced)
}
