use super::{Report, STARTER_TOML};
use crate::config::Config;
use crate::presets::{self, Preset, PresetName};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;

pub fn global(name: Option<PresetName>) -> Result<Report> {
    let path = crate::config::global_path().context("no platform config directory available")?;
    if name.is_none() && path.exists() {
        return Ok(skipped(path));
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if name.is_some() {
                "# Summoner global executor configuration.\n".to_string()
            } else {
                STARTER_TOML.to_string()
            }
        }
        Err(error) => return Err(error).context("reading global config"),
    };
    let updated = match name {
        Some(name) => install(&path, &existing, &presets::get(name)?)?,
        None => existing,
    };
    if path.exists() && updated == std::fs::read_to_string(&path)? {
        return Ok(skipped(path));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating global config directory")?;
    }
    std::fs::write(&path, updated).context("writing global config")?;
    Ok(Report {
        written: vec![path.display().to_string()],
        skipped: Vec::new(),
    })
}

fn install(path: &std::path::Path, existing: &str, preset: &Preset) -> Result<String> {
    let config: Config = toml::from_str(existing).with_context(|| {
        format!(
            "cannot install a preset into invalid config {}",
            path.display()
        )
    })?;
    if let Some(current) = config.executors.get(&preset.name)
        && current != &preset.backend
    {
        bail!(
            "{} already defines [executors.{}]; refusing to replace it (edit or rename it explicitly)",
            path.display(),
            preset.name
        );
    }
    if let Some(current) = config.executors.get(&preset.reviewer_name)
        && current != &preset.reviewer
    {
        bail!(
            "{} already defines [executors.{}]; refusing to replace it (edit or rename it explicitly)",
            path.display(),
            preset.reviewer_name
        );
    }

    let needs_default = config.default_executor.is_none();
    let needs_default_reviewer = config.default_reviewer.is_none();
    let needs_backend = !config.executors.contains_key(&preset.name);
    let needs_reviewer = !config.executors.contains_key(&preset.reviewer_name);
    let needs_auth_ack = !preset.auth_checked
        && [&preset.name, &preset.reviewer_name]
            .into_iter()
            .any(|name| !config.allow_unknown_auth.contains(name));
    if !needs_default
        && !needs_default_reviewer
        && !needs_backend
        && !needs_reviewer
        && !needs_auth_ack
    {
        return Ok(existing.to_string());
    }
    let mut updated = existing.to_string();
    if needs_default {
        updated = insert_root(&updated, &format!("default_executor = {:?}\n", preset.name));
    }
    if needs_default_reviewer {
        updated = insert_root(
            &updated,
            &format!("default_reviewer = {:?}\n", preset.reviewer_name),
        );
    }
    if needs_auth_ack {
        let mut names = config.allow_unknown_auth.clone();
        for name in [&preset.name, &preset.reviewer_name] {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        updated = replace_root_array(&updated, "allow_unknown_auth", &names)?;
    }
    if needs_backend || needs_reviewer {
        updated.push_str(&render_backends(preset, needs_backend, needs_reviewer)?);
    }
    toml::from_str::<Config>(&updated).context("generated preset config is invalid")?;
    Ok(updated)
}

fn replace_root_array(existing: &str, key: &str, values: &[String]) -> Result<String> {
    let value = values
        .iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let line = format!("{key} = [{value}]\n");
    let mut offset = 0;
    for current in existing.split_inclusive('\n') {
        let trimmed = current.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if trimmed.starts_with(&format!("{key} =")) {
            if !trimmed.contains(']') {
                bail!("{key} must use a single-line array before installing this preset");
            }
            return Ok(format!(
                "{}{}{}",
                &existing[..offset],
                line,
                &existing[offset + current.len()..]
            ));
        }
        offset += current.len();
    }
    Ok(insert_root(existing, &line))
}

fn insert_root(existing: &str, line: &str) -> String {
    let mut offset = 0;
    for current in existing.split_inclusive('\n') {
        if current.trim_start().starts_with('[') {
            break;
        }
        offset += current.len();
    }
    let mut output = String::with_capacity(existing.len() + line.len() + 1);
    output.push_str(&existing[..offset]);
    if offset > 0 && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(line);
    output.push_str(&existing[offset..]);
    output
}

fn render_backends(preset: &Preset, worker: bool, reviewer: bool) -> Result<String> {
    #[derive(Serialize)]
    struct Tables {
        executors: BTreeMap<String, crate::config::ExecutorBackend>,
    }
    let mut one = Tables {
        executors: BTreeMap::new(),
    };
    if worker {
        one.executors
            .insert(preset.name.clone(), preset.backend.clone());
    }
    if reviewer {
        one.executors
            .insert(preset.reviewer_name.clone(), preset.reviewer.clone());
    }
    let body = toml::to_string_pretty(&one).context("serializing embedded preset")?;
    Ok(format!(
        "\n# --- summoner preset schema 1: {} ---\n{}# --- end summoner preset: {} ---\n",
        preset.name, body, preset.name
    ))
}

fn skipped(path: std::path::PathBuf) -> Report {
    Report {
        written: Vec::new(),
        skipped: vec![path.display().to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_install_preserves_text_and_is_idempotent() {
        let path = std::path::Path::new("config.toml");
        let existing =
            "# keep this\nmax_parallel = 3\n\n[profiles.mine]\ndefault_executor = \"codex\"\n";
        let preset = presets::get(PresetName::Codex).unwrap();
        let installed = install(path, existing, &preset).unwrap();
        assert!(installed.contains("# keep this"));
        assert!(installed.contains("[profiles.mine]"));
        assert!(installed.contains("[executors.codex]"));
        assert_eq!(install(path, &installed, &preset).unwrap(), installed);
    }

    #[test]
    fn preset_install_preserves_existing_default_and_refuses_backend_conflicts() {
        let path = std::path::Path::new("config.toml");
        let preset = presets::get(PresetName::Codex).unwrap();
        let with_default = install(path, "default_executor = \"mine\"\n", &preset).unwrap();
        assert!(with_default.starts_with("default_executor = \"mine\""));
        let backend_error =
            install(path, "[executors.codex]\nargv = [\"mine\"]\n", &preset).unwrap_err();
        assert!(backend_error.to_string().contains("executors.codex"));
    }

    #[test]
    fn presets_append_sequentially_without_changing_the_first_default() {
        let path = std::path::Path::new("config.toml");
        let mut text = "# mine\n".to_string();
        for name in [PresetName::Codex, PresetName::Claude, PresetName::Kimi] {
            text = install(path, &text, &presets::get(name).unwrap()).unwrap();
        }
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.default_executor.as_deref(), Some("codex"));
        assert_eq!(config.default_reviewer.as_deref(), Some("codex-review"));
        for name in [
            "codex",
            "codex-review",
            "claude",
            "claude-review",
            "kimi",
            "kimi-review",
        ] {
            assert!(config.executors.contains_key(name), "missing {name}");
        }
        assert_eq!(config.allow_unknown_auth, ["kimi", "kimi-review"]);
        let again = install(path, &text, &presets::get(PresetName::Kimi).unwrap()).unwrap();
        assert_eq!(again, text);
    }

    #[test]
    fn preset_install_preserves_crlf_table_boundaries() {
        let path = std::path::Path::new("config.toml");
        let existing = "# keep\r\n[profiles.mine]\r\ndefault_executor = \"custom\"\r\n";
        let installed = install(path, existing, &presets::get(PresetName::Codex).unwrap()).unwrap();
        assert!(installed.contains("# keep\r\n"));
        assert!(installed.contains("[profiles.mine]\r\n"));
        toml::from_str::<Config>(&installed).unwrap();
    }
}
