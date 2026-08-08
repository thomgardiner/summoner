use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_executor: Option<String>,
    pub default_verify_profile: Option<String>,
    pub max_parallel: Option<usize>,
    pub order_timeout_secs: Option<u64>,
    pub executors: BTreeMap<String, Executor>,
    pub verification: Verification,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Executor {
    pub argv: Vec<String>,
    pub prompt: PromptRouting,
    pub timeout_secs: Option<u64>,
    pub env_required: Vec<String>,
}

impl Default for Executor {
    fn default() -> Self {
        Self {
            argv: Vec::new(),
            prompt: PromptRouting::Arg,
            timeout_secs: None,
            env_required: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptRouting {
    #[default]
    Arg,
    Stdin,
    File,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Verification {
    pub profiles: BTreeMap<String, VerificationProfile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationProfile {
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTable {
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    Table(CommandTable),
    Array(Vec<String>),
}

impl CommandSpec {
    pub fn argv(&self) -> &[String] {
        match self {
            Self::Table(table) => &table.argv,
            Self::Array(argv) => argv,
        }
    }
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let mut config = Config::default();
    if let Some(path) = global_path()
        && let Some(value) = read_optional(&path)?
    {
        merge(&mut config, value);
    }
    if let Some(path) = repo_config_path() {
        merge(&mut config, read(&path)?);
    }
    if let Some(path) = explicit {
        merge(&mut config, read(path)?);
    }
    validate(&config)?;
    Ok(config)
}

pub fn global_path() -> Option<PathBuf> {
    #[cfg(windows)]
    let root = env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Roaming"))
    });
    #[cfg(not(windows))]
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    root.map(|p| p.join("summoner").join("config.toml"))
}

pub fn repo_config_path() -> Option<PathBuf> {
    let cwd = env::current_dir().ok()?;
    cwd.ancestors()
        .map(|dir| dir.join(".summoner.toml"))
        .find(|path| path.is_file())
}

fn read(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
}

fn read_optional(path: &Path) -> Result<Option<Config>> {
    match read(path) {
        Ok(config) => Ok(Some(config)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn merge(base: &mut Config, over: Config) {
    if over.default_executor.is_some() {
        base.default_executor = over.default_executor;
    }
    if over.default_verify_profile.is_some() {
        base.default_verify_profile = over.default_verify_profile;
    }
    if over.max_parallel.is_some() {
        base.max_parallel = over.max_parallel;
    }
    if over.order_timeout_secs.is_some() {
        base.order_timeout_secs = over.order_timeout_secs;
    }
    for (name, executor) in over.executors {
        base.executors.insert(name, executor);
    }
    if !over.verification.profiles.is_empty() {
        for (name, profile) in over.verification.profiles {
            base.verification.profiles.insert(name, profile);
        }
    }
}

pub fn validate(config: &Config) -> Result<()> {
    for (name, executor) in &config.executors {
        if executor.argv.is_empty() || executor.argv[0].trim().is_empty() {
            bail!("executor {name:?} has empty argv");
        }
    }
    for (name, profile) in &config.verification.profiles {
        if profile.commands.is_empty() {
            bail!("verification profile {name:?} has no commands");
        }
        for (index, command) in profile.commands.iter().enumerate() {
            if command.argv().is_empty() || command.argv()[0].trim().is_empty() {
                bail!("verification profile {name:?} command {index} has empty argv");
            }
        }
    }
    Ok(())
}

pub fn max_parallel(config: &Config, override_jobs: Option<usize>) -> Result<usize> {
    let jobs = override_jobs.or(config.max_parallel).unwrap_or(5);
    if jobs == 0 {
        bail!("jobs/max_parallel must be greater than zero");
    }
    Ok(jobs)
}

pub fn timeout(config: &Config, executor: &Executor, order: Option<u64>) -> u64 {
    order
        .or(executor.timeout_secs)
        .or(config.order_timeout_secs)
        .unwrap_or(600)
        .clamp(1, 31_536_000)
}

#[cfg(test)]
mod tests {
    use super::{Config, max_parallel};

    #[test]
    fn max_parallel_defaults_to_five() {
        assert_eq!(max_parallel(&Config::default(), None).unwrap(), 5);
    }
}
