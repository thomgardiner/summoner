//! Idempotent repository initialization.

#[path = "init_global.rs"]
mod global;
#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;

use anyhow::{Context, Result};
pub use global::global;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::grove::GroveCli;
use crate::presets::PresetName;

const MARKER: &str = "<!-- summoner:agents:v1 -->";
const END_MARKER: &str = "<!-- summoner:agents:end -->";
const AGENTS_SECTION: &str = include_str!("../assets/agents-section.md");
const SKILL: &str = include_str!("../assets/skill.md");
const STARTER_TOML: &str = include_str!("../assets/summoner-starter.toml");
const EXAMPLE_ORDER: &str = include_str!("../assets/example-order.toml");
const GROVE_DEMO: &str = include_str!("../assets/grove-demo.toml");

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

pub(crate) fn onboard(
    workspace: &Path,
    write_global: bool,
    preset: Option<PresetName>,
) -> Result<Report> {
    let resolved = crate::config::load()?;
    let mut snapshot = (write_global || preset.is_some())
        .then(GlobalSnapshot::capture)
        .transpose()?;
    let mut report = if write_global || preset.is_some() {
        let report = global(preset)?;
        if let Some(snapshot) = &mut snapshot {
            snapshot.record_written()?;
        }
        report
    } else {
        Report::default()
    };
    let grove = GroveCli::new(resolved.config.grove_bin());
    match example(workspace, false, &grove) {
        Ok(example) => {
            report.merge(example);
            Ok(report)
        }
        Err(error) => match snapshot {
            Some(snapshot) => Err(snapshot.rollback(error)),
            None => Err(error),
        },
    }
}

struct GlobalSnapshot {
    path: PathBuf,
    original: Option<Vec<u8>>,
    written: Option<Vec<u8>>,
}

impl GlobalSnapshot {
    fn capture() -> Result<Self> {
        let path =
            crate::config::global_path().context("no platform config directory available")?;
        let original = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("reading global config snapshot"),
        };
        Ok(Self {
            path,
            original,
            written: None,
        })
    }

    fn record_written(&mut self) -> Result<()> {
        self.written = Some(
            std::fs::read(&self.path).context("reading generated global config for rollback")?,
        );
        Ok(())
    }

    fn rollback(self, cause: anyhow::Error) -> anyhow::Error {
        self.rollback_with(cause, global::write_atomic)
    }

    fn rollback_with(
        self,
        cause: anyhow::Error,
        restore: impl FnOnce(&Path, &[u8]) -> Result<()>,
    ) -> anyhow::Error {
        match self.restore_if_unchanged(restore) {
            Ok(()) => cause,
            Err(error) => cause.context(format!("{error:#}")),
        }
    }

    fn restore_if_unchanged(self, restore: impl FnOnce(&Path, &[u8]) -> Result<()>) -> Result<()> {
        let written = self
            .written
            .context("global rollback state was not recorded")?;
        let current = match std::fs::read(&self.path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("checking global config before rollback"),
        };
        if current.as_deref() != Some(written.as_slice()) {
            anyhow::bail!(
                "global config {} changed concurrently; preserved its current state — reconcile it manually before retrying",
                self.path.display()
            );
        }
        match self.original {
            Some(contents) => restore(&self.path, &contents).context("restoring global config"),
            None => std::fs::remove_file(self.path).context("removing generated global config"),
        }
    }
}

pub fn example(workspace: &Path, refresh: bool, grove: &GroveCli) -> Result<Report> {
    let mut report = ensure_demo_profile(workspace)?;
    if report.written.iter().any(|path| path == ".grove.toml")
        && !workspace.join("Cargo.lock").is_file()
    {
        if let Err(error) = grove.cargo_generate_lockfile(workspace) {
            std::fs::remove_file(workspace.join(".grove.toml"))?;
            let lockfile = workspace.join("Cargo.lock");
            if lockfile.exists() {
                std::fs::remove_file(lockfile)?;
            }
            return Err(error);
        }
        report.written.push("Cargo.lock".to_string());
    }
    report.merge(init(workspace, refresh)?);
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
    let profiles = crate::config::grove_profiles(workspace)?;
    let profile = profiles.selected.context(
        "the existing .grove.toml has no single required usable verification profile; select exactly one required profile before creating the example",
    )?;
    let value = EXAMPLE_ORDER.replace(
        "verify_profile = \"rust-check\"",
        &format!("verify_profile = {profile:?}"),
    );
    Ok(value)
}

fn ensure_demo_profile(workspace: &Path) -> Result<Report> {
    let mut report = Report::default();
    let profiles = crate::config::grove_profiles(workspace)?;
    if profiles.path.is_some() {
        if profiles.selected.is_none() {
            anyhow::bail!(
                "the existing .grove.toml has no single required usable verification profile; configure exactly one required profile before creating the example"
            );
        }
        report.skipped.push(".grove.toml".to_string());
        return Ok(report);
    }
    if !workspace.join("Cargo.toml").is_file() {
        anyhow::bail!("the example requires a Rust workspace with Cargo.toml")
    }
    std::fs::write(workspace.join(".grove.toml"), GROVE_DEMO).context("writing .grove.toml")?;
    report.written.push(".grove.toml".to_string());
    Ok(report)
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
    // The skill file is Claude Code furniture. Writing it unconditionally put
    // a `.claude/` directory into every repository regardless of which harness
    // the user actually runs, which is exactly the vendor-specific residue a
    // neutral tool must not leave. It is written only where Claude Code is
    // already in evidence; every other harness reads the same contract from
    // AGENTS.md.
    let claude_present =
        workspace.join(".claude").is_dir() || workspace.join("CLAUDE.md").is_file();
    if !claude_present {
        report
            .skipped
            .push(".claude/skills/summoner/SKILL.md (no Claude Code in this repo)".to_string());
        return Ok(());
    }
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
