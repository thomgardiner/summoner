//! First-run developer ergonomics: user skills + optional preset / wizard.
//!
//! `summoner setup` is what the installer README points at so harnesses can
//! invoke Summoner without hunting for docs. No model is selected by default.

use crate::init;
use crate::presets::PresetName;
use crate::skills;
use crate::wizard::{self, Scope};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub skills: skills::Report,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global: Option<init::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wizard: Option<wizard::Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_cleared: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<init::Report>,
    pub next_steps: Vec<String>,
}

pub struct SetupArgs {
    pub preset: Option<PresetName>,
    /// Force session-only when applying `--preset`.
    pub session: bool,
    /// Force interactive wizard even if a preset flag is absent.
    pub wizard: bool,
    pub clear_session: bool,
    pub refresh: bool,
    pub repo: bool,
}

/// Install harness skills, optional recipe (session or permanent), optional repo contracts.
pub fn setup(workspace: &Path, args: SetupArgs) -> Result<Report> {
    let skills = skills::install_user_skills(args.refresh)?;
    let mut session_cleared = None;
    if args.clear_session {
        session_cleared = wizard::clear_session()?;
    }

    let mut wizard_report = None;
    let mut global = None;

    if let Some(preset) = args.preset {
        let scope = if args.session {
            Scope::Session
        } else {
            Scope::Permanent
        };
        wizard_report = Some(wizard::apply(preset, scope)?);
    } else if args.wizard || (!args.clear_session && !has_any_executor()?) {
        // Interactive when forced, or when no executor is configured yet.
        match wizard::run_interactive(true) {
            Ok(report) => wizard_report = Some(report),
            Err(error) if args.wizard => return Err(error),
            Err(_) => {
                // Non-TTY without preset: still ensure a model-free global skeleton.
                global = Some(init::global(None)?);
            }
        }
    } else {
        // Skills-only / refresh path: never inject a model recipe.
        global = Some(init::global(None)?);
    }

    let repo_report = if args.repo {
        Some(init::init(workspace, args.refresh)?)
    } else {
        None
    };

    let mut next_steps = skills.next_steps.clone();
    if let Some(wizard) = &wizard_report {
        next_steps.extend(wizard.next_steps.iter().cloned());
    } else if wizard::needs_executor(&crate::config::load()?.config) {
        next_steps.insert(
            0,
            "Choose a model: `summoner setup` (wizard) or `summoner setup --preset <codex|claude|kimi>`".into(),
        );
        next_steps.insert(
            1,
            "Session-only recipe: `summoner setup --preset <name> --session`".into(),
        );
    }
    if !args.repo {
        next_steps
            .push("In a project: summoner setup --repo   # AGENTS.md + sample contracts".into());
    }
    next_steps.push("Verify: summoner doctor".into());

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
        wizard: wizard_report,
        session_cleared,
        repo: repo_report,
        next_steps,
    })
}

fn has_any_executor() -> Result<bool> {
    let resolved = crate::config::load()?;
    Ok(!wizard::needs_executor(&resolved.config))
}
