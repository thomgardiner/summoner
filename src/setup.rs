//! First-run developer ergonomics: user skills + optional preset + next steps.
//!
//! `summoner setup` is what the installer README points at so Claude/Codex/Grok
//! can invoke Summoner without hunting for docs.

use crate::init;
use crate::presets::PresetName;
use crate::skills;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub skills: skills::Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global: Option<init::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<init::Report>,
    pub next_steps: Vec<String>,
}

/// Install harness skills, optional global preset, optional repo contracts.
pub fn setup(
    workspace: &Path,
    preset: Option<PresetName>,
    refresh: bool,
    repo: bool,
) -> Result<Report> {
    let skills = skills::install_user_skills(refresh)?;
    // Always ensure a personal config skeleton exists; apply preset when named.
    let global = Some(init::global(preset)?);
    let repo_report = if repo {
        Some(init::init(workspace, refresh)?)
    } else {
        None
    };

    let mut next_steps = skills.next_steps.clone();
    if preset.is_none() {
        next_steps.insert(
            0,
            "Pick a model recipe: summoner setup --preset codex  (or claude / kimi)".into(),
        );
    }
    if !repo {
        next_steps
            .push("In a project: summoner setup --repo   # AGENTS.md + sample contracts".into());
    }
    next_steps.push("Verify: summoner doctor".into());

    // Human card for first-run eyes; JSON report still on stdout for machines.
    eprintln!(
        "summoner setup: skills written={}, skipped={}",
        skills.written.len(),
        skills.skipped.len()
    );
    for step in &next_steps {
        eprintln!("  → {step}");
    }

    Ok(Report {
        skills,
        global,
        repo: repo_report,
        next_steps,
    })
}
