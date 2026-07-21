use super::{Config, Resolved, merge};
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub fn global_path() -> Option<PathBuf> {
    global_path_for(
        cfg!(windows),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("APPDATA"),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

pub(crate) struct GroveProfiles {
    pub path: Option<PathBuf>,
    pub names: BTreeSet<String>,
    pub selected: Option<String>,
}

pub(crate) fn grove_profiles(cwd: &Path) -> Result<GroveProfiles> {
    let Some(path) = cwd
        .ancestors()
        .map(|dir| dir.join(".grove.toml"))
        .find(|path| path.exists())
    else {
        return Ok(GroveProfiles {
            path: None,
            names: BTreeSet::new(),
            selected: None,
        });
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let verification = value.get("verification");
    let names = usable_profiles(verification);
    let required = verification
        .and_then(|value| value.get("required"))
        .and_then(toml::Value::as_array)
        .filter(|items| items.len() == 1)
        .and_then(|items| items.first())
        .and_then(toml::Value::as_str)
        .filter(|name| names.contains(*name))
        .map(String::from);
    let selected = required;
    Ok(GroveProfiles {
        path: Some(path),
        names,
        selected,
    })
}

fn usable_profiles(verification: Option<&toml::Value>) -> BTreeSet<String> {
    verification
        .and_then(|value| value.get("profiles"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|profiles| profiles.iter())
        .filter(|(_, profile)| {
            profile
                .get("commands")
                .and_then(toml::Value::as_array)
                .is_some_and(|commands| !commands.is_empty())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

fn global_path_for(
    windows: bool,
    xdg: Option<OsString>,
    appdata: Option<OsString>,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    let root = if windows {
        appdata.map(PathBuf::from).or_else(|| {
            user_profile
                .or(home)
                .map(PathBuf::from)
                .map(|path| path.join("AppData").join("Roaming"))
        })
    } else {
        xdg.map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                home.or(user_profile)
                    .map(PathBuf::from)
                    .map(|path| path.join(".config"))
            })
    }?;
    Some(root.join("summoner").join("config.toml"))
}

pub(super) fn read(path: &Path) -> Result<Option<Config>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    toml::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

pub(super) fn repo_config_path_from(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join(".summoner.toml"))
        .find(|path| path.exists())
}

pub fn select_profile(config: &mut Config, flag: Option<&str>) -> anyhow::Result<Option<String>> {
    let env = std::env::var("SUMMONER_PROFILE")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let explicit = flag
        .map(String::from)
        .or(env)
        .or_else(|| config.profile.clone());
    let name = match explicit {
        Some(name) => explicit_profile(config, name)?,
        None => match detected_profile(config) {
            Some(name) => name,
            None => return Ok(None),
        },
    };
    let profile = config.profiles[&name].clone();
    if let Some(executor) = profile.default_executor {
        config.default_executor = Some(executor);
    }
    if let Some(reviewer) = profile.default_reviewer {
        config.default_reviewer = Some(reviewer);
    }
    Ok(Some(name))
}

fn explicit_profile(config: &Config, name: String) -> anyhow::Result<String> {
    if config.profiles.contains_key(&name) {
        return Ok(name);
    }
    let available = if config.profiles.is_empty() {
        "none".to_string()
    } else {
        config
            .profiles
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    anyhow::bail!("profile {name:?} is not defined (configured profiles: {available})")
}

fn detected_profile(config: &Config) -> Option<String> {
    let has = |variable: &str| std::env::var_os(variable).is_some();
    if has("CLAUDECODE") && has("CODEX_SANDBOX") && !config.profiles.is_empty() {
        eprintln!(
            "summoner: both CLAUDECODE and CODEX_SANDBOX are set (nested harnesses); \
             profile detection is ambiguous — pass --profile, set SUMMONER_PROFILE, \
             or pin profile = \"<name>\" in config"
        );
    }
    detect_orchestrator(has)
        .filter(|name| config.profiles.contains_key(*name))
        .map(String::from)
}

pub(super) fn detect_orchestrator(has_var: impl Fn(&str) -> bool) -> Option<&'static str> {
    match (has_var("CLAUDECODE"), has_var("CODEX_SANDBOX")) {
        (true, false) => Some("claude"),
        (false, true) => Some("codex"),
        _ => None,
    }
}

pub fn load() -> Result<Resolved> {
    let cwd = std::env::current_dir().context("resolving current directory for configuration")?;
    load_from(&cwd)
}

pub(super) fn load_from(cwd: &Path) -> Result<Resolved> {
    let mut config = Config::default();
    let mut sources = Vec::new();
    if let Some(path) = global_path()
        && let Some(global) = read(&path)?
    {
        merge(&mut config, global);
        sources.push(path.display().to_string());
    }
    if let Some(path) = repo_config_path_from(cwd)
        && let Some(repo) = read(&path)?
    {
        if !repo.allow_unknown_auth.is_empty() {
            anyhow::bail!(
                "{} cannot set allow_unknown_auth; authentication acknowledgements belong in the personal global config or the CLI",
                path.display()
            );
        }
        merge(&mut config, repo);
        sources.push(path.display().to_string());
    }
    Ok(Resolved {
        sources,
        selected_profile: None,
        config,
    })
}

pub(super) fn env_bool(key: &str) -> Option<bool> {
    match std::env::var(key)
        .ok()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_follow_each_platforms_native_convention() {
        let xdg = Some(OsString::from("/xdg"));
        let appdata = Some(OsString::from("C:\\Users\\me\\AppData\\Roaming"));
        let home = Some(OsString::from("/home/me"));
        let profile = Some(OsString::from("C:\\Users\\me"));
        assert_eq!(
            global_path_for(
                false,
                xdg.clone(),
                appdata.clone(),
                home.clone(),
                profile.clone()
            ),
            Some(PathBuf::from("/xdg/summoner/config.toml"))
        );
        assert_eq!(
            global_path_for(true, xdg, appdata, home, profile),
            Some(PathBuf::from("C:\\Users\\me\\AppData\\Roaming").join("summoner/config.toml"))
        );
    }

    #[test]
    fn config_paths_have_native_fallbacks() {
        assert_eq!(
            global_path_for(false, None, None, Some("/home/me".into()), None),
            Some(PathBuf::from("/home/me/.config/summoner/config.toml"))
        );
        assert_eq!(
            global_path_for(true, None, None, None, Some("C:\\Users\\me".into())),
            Some(
                PathBuf::from("C:\\Users\\me")
                    .join("AppData")
                    .join("Roaming")
                    .join("summoner/config.toml")
            )
        );
    }

    #[test]
    fn only_profiles_with_commands_are_usable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".grove.toml"),
            "[verification]\nrequired = [\"empty\"]\n\
             [verification.profiles.empty]\ncommands = []\n\
             [verification.profiles.real]\ncommands = [{ argv = [\"true\"] }]\n",
        )
        .unwrap();
        let profiles = grove_profiles(dir.path()).unwrap();
        assert_eq!(profiles.names, BTreeSet::from(["real".to_string()]));
        assert_eq!(profiles.selected, None);
    }
}
