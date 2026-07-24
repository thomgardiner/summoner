//! Interactive executor setup: choose a recipe for this session or permanently.
//!
//! Summoner ships no default model. Presets (codex / claude / kimi) are optional
//! recipes applied only when the operator picks them — never assumed present.

use crate::config;
use crate::init;
use crate::presets::{self, PresetName};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::{self, BufRead, IsTerminal, Write};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `$XDG_RUNTIME_DIR` / cache session file; does not touch global config.
    Session,
    /// Personal global config (all future summoner invocations).
    Permanent,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub scope: String,
    pub executor: String,
    pub reviewer: Option<String>,
    pub path: String,
    pub available: Vec<Available>,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Available {
    pub name: String,
    pub on_path: bool,
    pub binary: String,
}

/// Scan embedded presets and report which CLI binaries are on PATH.
pub fn scan() -> Result<Vec<Available>> {
    let mut out = Vec::new();
    for name in [PresetName::Codex, PresetName::Claude, PresetName::Kimi] {
        let preset = presets::get(name)?;
        let binary = preset
            .backend
            .argv
            .first()
            .cloned()
            .unwrap_or_else(|| preset.name.clone());
        out.push(Available {
            name: preset.name,
            on_path: presets::on_path(&binary),
            binary,
        });
    }
    Ok(out)
}

/// Apply a named preset to session or global config without a prompt.
pub fn apply(name: PresetName, scope: Scope) -> Result<Report> {
    let preset = presets::get(name)?;
    let path = match scope {
        Scope::Session => {
            let path = config::session_path().context("no session config directory available")?;
            init::write_preset_config(&path, &preset)?;
            path
        }
        Scope::Permanent => {
            let path = config::global_path().context("no platform config directory available")?;
            init::global(Some(name))?;
            path
        }
    };
    Ok(report_for(
        scope,
        &preset.name,
        Some(&preset.reviewer_name),
        path,
    ))
}

/// Clear the session-only config if present.
pub fn clear_session() -> Result<Option<String>> {
    let Some(path) = config::session_path() else {
        return Ok(None);
    };
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        Ok(Some(path.display().to_string()))
    } else {
        Ok(None)
    }
}

/// Interactive wizard when stdin/stdout are terminals; otherwise explain non-TTY use.
pub fn run_interactive(prefer_on_path: bool) -> Result<Report> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "no interactive terminal: pick a recipe explicitly with\n  \
             summoner setup --preset <codex|claude|kimi>           # save to personal config\n  \
             summoner setup --preset <codex|claude|kimi> --session # this session only\n  \
             summoner setup --clear-session"
        );
    }
    let available = scan()?;
    let mut out = io::stdout();
    writeln!(
        out,
        "Summoner needs a model CLI. Nothing is selected by default."
    )?;
    writeln!(out, "Recipes found in this package (binary on PATH?):")?;
    for (i, item) in available.iter().enumerate() {
        let mark = if item.on_path { "yes" } else { "no " };
        writeln!(
            out,
            "  {}. {:<8}  on PATH: {}  ({})",
            i + 1,
            item.name,
            mark,
            item.binary
        )?;
    }
    writeln!(out, "  0. cancel")?;
    out.flush()?;

    let default_idx = if prefer_on_path {
        available.iter().position(|a| a.on_path)
    } else {
        None
    };
    let prompt = match default_idx {
        Some(i) => format!(
            "Executor recipe [1-{}, default {}]: ",
            available.len(),
            i + 1
        ),
        None => format!("Executor recipe [1-{}]: ", available.len()),
    };
    let choice = read_choice(&prompt, available.len(), default_idx.map(|i| i + 1))?;
    if choice == 0 {
        bail!("setup cancelled");
    }
    let selected = &available[choice - 1];
    if !selected.on_path {
        writeln!(
            out,
            "warning: {:?} is not on PATH; install it or doctor will refuse fleets",
            selected.binary
        )?;
    }

    let scope = read_scope()?;
    let name = match selected.name.as_str() {
        "codex" => PresetName::Codex,
        "claude" => PresetName::Claude,
        "kimi" => PresetName::Kimi,
        other => bail!("unknown preset {other}"),
    };
    let report = apply(name, scope)?;
    writeln!(
        out,
        "configured executor {:?} (reviewer {:?}) → {}",
        report.executor,
        report.reviewer.as_deref().unwrap_or("none"),
        report.path
    )?;
    for step in &report.next_steps {
        writeln!(out, "  → {step}")?;
    }
    Ok(report)
}

fn read_scope() -> Result<Scope> {
    let mut out = io::stdout();
    writeln!(out, "Where should this apply?")?;
    writeln!(
        out,
        "  1. this session only (session config; cleared with --clear-session)"
    )?;
    writeln!(out, "  2. all sessions (personal global config)")?;
    out.flush()?;
    let n = read_choice("Scope [1-2, default 1]: ", 2, Some(1))?;
    Ok(match n {
        1 => Scope::Session,
        2 => Scope::Permanent,
        _ => unreachable!(),
    })
}

fn read_choice(prompt: &str, max: usize, default: Option<usize>) -> Result<usize> {
    let mut out = io::stdout();
    let stdin = io::stdin();
    loop {
        write!(out, "{prompt}")?;
        out.flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if let Some(value) = default {
                return Ok(value);
            }
            continue;
        }
        if let Ok(n) = trimmed.parse::<usize>()
            && (n == 0 || (1..=max).contains(&n))
        {
            return Ok(n);
        }
        writeln!(out, "enter a number from 0 to {max}")?;
    }
}

fn report_for(
    scope: Scope,
    executor: &str,
    reviewer: Option<&str>,
    path: std::path::PathBuf,
) -> Report {
    let available = scan().unwrap_or_default();
    let mut next_steps = vec![
        "summoner doctor".into(),
        "summoner plan orders/… && summoner run orders/…".into(),
    ];
    if scope == Scope::Session {
        next_steps
            .push("session-only: re-run `summoner setup` next session, or --clear-session".into());
    }
    Report {
        scope: match scope {
            Scope::Session => "session".into(),
            Scope::Permanent => "permanent".into(),
        },
        executor: executor.into(),
        reviewer: reviewer.map(str::to_string),
        path: path.display().to_string(),
        available,
        next_steps,
    }
}

/// True when config has no default executor and no named executors — fleets cannot start.
pub fn needs_executor(config: &crate::config::Config) -> bool {
    config.default_executor().is_none() && config.executors.is_empty()
}

/// If fleets cannot run and a TTY is available, offer the wizard once.
pub fn ensure_executor_or_wizard(config: &crate::config::Config) -> Result<()> {
    if !needs_executor(config) {
        return Ok(());
    }
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        eprintln!("summoner: no executor configured — launching setup wizard");
        run_interactive(true)?;
        // Caller must reload config after this returns.
        return Ok(());
    }
    bail!(
        "no executor configured. Run `summoner setup` (interactive), or\n  \
         summoner setup --preset <codex|claude|kimi>\n  \
         summoner setup --preset <name> --session   # this process environment only"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lists_all_shipped_recipes() {
        let available = scan().unwrap();
        let names: Vec<_> = available.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["codex", "claude", "kimi"]);
    }

    #[test]
    fn needs_executor_when_empty() {
        assert!(needs_executor(&crate::config::Config::default()));
    }
}
