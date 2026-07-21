use crate::config::ExecutorBackend;
use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DATA: &str = include_str!("../assets/summoner-presets.toml");

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PresetName {
    Codex,
    Claude,
    Kimi,
}

impl PresetName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Clone, Deserialize)]
pub struct Preset {
    pub name: String,
    pub backend: ExecutorBackend,
    pub reviewer_name: String,
    pub reviewer: ExecutorBackend,
    #[serde(default)]
    pub health_argv: Vec<String>,
    pub health_label: String,
    pub auth_checked: bool,
    pub setup_hint: String,
}

#[derive(Deserialize)]
struct Catalog {
    schema_version: u64,
    presets: Vec<Preset>,
}

fn catalog() -> Result<Catalog> {
    let catalog: Catalog = toml::from_str(DATA).context("parsing embedded executor presets")?;
    if catalog.schema_version != 1 {
        bail!(
            "unsupported embedded preset schema {}",
            catalog.schema_version
        );
    }
    Ok(catalog)
}

pub fn get(name: PresetName) -> Result<Preset> {
    let wanted = name.as_str();
    catalog()?
        .presets
        .into_iter()
        .find(|preset| preset.name == wanted)
        .with_context(|| format!("embedded preset {wanted:?} is missing"))
}

pub fn for_executor(name: &str, backend: &ExecutorBackend) -> Result<Option<Preset>> {
    Ok(catalog()?.presets.into_iter().find(|preset| {
        (preset.name == name && preset.backend == *backend)
            || (preset.reviewer_name == name && preset.reviewer == *backend)
    }))
}

pub fn on_path(binary: &str) -> bool {
    locate(binary).is_some()
}

/// First executable PATH candidate for `binary`, honoring Windows PATHEXT
/// variants. Spawning must use this resolved path: `Command::new` with a bare
/// name cannot start `.cmd`/`.bat` shims (npm-installed CLIs) on Windows.
pub fn locate(binary: &str) -> Option<std::path::PathBuf> {
    let path = Path::new(binary);
    if path.is_absolute() || binary.contains('/') || binary.contains('\\') {
        return candidates(path, cfg!(windows))
            .into_iter()
            .find(|path| executable(path));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            candidates(&dir.join(binary), cfg!(windows))
                .into_iter()
                .find(|path| executable(path))
        })
    })
}

pub fn health(argv: &[String]) -> std::result::Result<(), String> {
    let Some(binary) = argv.first() else {
        return Err("empty diagnostic command".to_string());
    };
    let program = locate(binary).map_or_else(|| Path::new(binary).to_path_buf(), |path| path);
    let mut child = Command::new(&program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start diagnostic: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("diagnostic exited with {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("diagnostic exceeded 5 seconds".to_string());
            }
            Err(error) => return Err(format!("diagnostic wait failed: {error}")),
        }
    }
}

fn candidates(path: &Path, windows: bool) -> Vec<PathBuf> {
    if !windows || path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| path.with_extension(extension.trim_start_matches('.')))
        .collect()
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptRouting;
    use std::path::Path;

    #[test]
    fn catalog_matches_cli_names_and_has_no_bypass_flags() {
        let catalog = catalog().unwrap();
        let names: Vec<_> = catalog
            .presets
            .iter()
            .map(|preset| preset.name.as_str())
            .collect();
        assert_eq!(names, ["codex", "claude", "kimi"]);
        for preset in catalog.presets {
            let joined = [
                preset.backend.argv.join(" "),
                preset.reviewer.argv.join(" "),
            ]
            .join(" ");
            assert!(
                preset
                    .reviewer
                    .argv
                    .iter()
                    .all(|arg| !arg.contains("{git_common_dir}")),
                "{} reviewer reaches the authoritative Git dir",
                preset.name
            );
            for forbidden in [
                "dangerously-bypass",
                "bypassPermissions",
                "danger-full-access",
                "--yolo",
            ] {
                assert!(
                    !joined.contains(forbidden),
                    "{} contains {forbidden}",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn fake_binaries_receive_expanded_safe_argv_and_prompt_routing() {
        for name in [PresetName::Codex, PresetName::Claude, PresetName::Kimi] {
            let preset = get(name).unwrap();
            let mut template = preset.backend.argv.clone();
            template[0] = "fake-model".to_string();
            let argv = crate::executor::expand(
                &template,
                "DEMO PROMPT",
                Path::new("/fake/worktree"),
                Path::new("/fake/common.git"),
                Path::new("/fake/order.toml"),
                Path::new("/fake/prompt.md"),
                "",
            );
            assert_eq!(argv.first().map(String::as_str), Some("fake-model"));
            assert!(argv.iter().all(|arg| !arg.contains("{worktree}")));
            match preset.backend.routing() {
                PromptRouting::Arg => assert!(argv.iter().any(|arg| arg == "DEMO PROMPT")),
                PromptRouting::Stdin => assert!(argv.iter().all(|arg| arg != "DEMO PROMPT")),
                PromptRouting::File => panic!("shipped presets do not use file routing"),
            }
            let mut review = preset.reviewer.argv.clone();
            review[0] = "fake-reviewer".to_string();
            let review = crate::executor::expand(
                &review,
                "REVIEW PROMPT",
                Path::new("/fake/worktree"),
                Path::new("/fake/common.git"),
                Path::new("/fake/order.toml"),
                Path::new("/fake/review.md"),
                "",
            );
            assert_eq!(review.first().map(String::as_str), Some("fake-reviewer"));
            assert!(review.iter().all(|arg| !arg.contains("{worktree}")));
        }
        let kimi = get(PresetName::Kimi).unwrap();
        assert_eq!(
            kimi.backend.argv,
            [
                "kimi",
                "--auto",
                "--add-dir",
                "{git_common_dir}",
                "--prompt",
                "{prompt}"
            ]
        );
    }

    #[test]
    fn windows_candidates_use_pathext_shapes() {
        let paths = candidates(Path::new("C:/bin/codex"), true);
        assert!(paths.iter().any(|path| path.extension().is_some()));
        assert_eq!(
            candidates(Path::new("/bin/codex"), false),
            [PathBuf::from("/bin/codex")]
        );
    }

    #[test]
    fn bounded_diagnostic_rejects_empty_commands() {
        assert!(health(&[]).unwrap_err().contains("empty"));
    }

    #[test]
    fn custom_backend_reusing_a_preset_name_gets_no_vendor_probe() {
        let mut custom = get(PresetName::Codex).unwrap().backend;
        custom.argv = vec!["custom-agent".to_string()];
        assert!(for_executor("codex", &custom).unwrap().is_none());
    }
}
