//! Optional configuration. Every setting has a default; a config file or an
//! environment variable overrides it. Precedence, highest first:
//!
//!   SUMMONER_* env  >  ./.summoner.toml (per repo)  >  ~/.config/summoner/config.toml  >  default
//!
//! Executors are pure data: named argv templates. Summoner compiles in no vendor
//! knowledge; presets ship only in the starter file `summoner init` writes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_executor: Option<String>,
    pub max_parallel: Option<usize>,
    pub default_verify_profile: Option<String>,
    pub order_timeout_secs: Option<u64>,
    pub grove_bin: Option<String>,
    pub keep_failed_worktrees: Option<bool>,
    pub executors: BTreeMap<String, ExecutorBackend>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBackend {
    /// Literal program and arguments, expanded per order. Placeholders:
    /// `{prompt}`, `{worktree}`, `{order_file}`, `{prompt_file}`. Elements are
    /// never shell-joined, so vendor greedy-flag orderings survive verbatim.
    pub argv: Vec<String>,
    #[serde(default)]
    pub prompt: PromptRouting,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub env_required: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptRouting {
    /// Substituted into the `{prompt}` argv placeholder.
    #[default]
    Arg,
    /// Piped to the executor's stdin, then closed.
    Stdin,
    /// Written to the run directory, path substituted into `{prompt_file}`.
    File,
}

/// The merged config plus where it came from, for `summoner config`.
#[derive(Serialize)]
pub struct Resolved {
    pub sources: Vec<String>,
    #[serde(flatten)]
    pub config: Config,
}

impl Config {
    pub fn default_executor(&self) -> Option<String> {
        std::env::var("SUMMONER_DEFAULT_EXECUTOR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.default_executor.clone())
    }

    pub fn max_parallel(&self) -> usize {
        std::env::var("SUMMONER_MAX_PARALLEL")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(self.max_parallel)
            .filter(|n| *n > 0)
            .unwrap_or(2)
    }

    pub fn order_timeout_secs(&self) -> u64 {
        std::env::var("SUMMONER_ORDER_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(self.order_timeout_secs)
            .filter(|n| *n > 0)
            .unwrap_or(600)
    }

    pub fn grove_bin(&self) -> String {
        std::env::var("SUMMONER_GROVE_BIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.grove_bin.clone())
            .unwrap_or_else(|| "grove".to_string())
    }

    pub fn keep_failed_worktrees(&self) -> bool {
        env_bool("SUMMONER_KEEP_FAILED_WORKTREES")
            .or(self.keep_failed_worktrees)
            .unwrap_or(false)
    }
}

/// The global config file path, whether or not it exists.
pub fn global_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .map(|d| d.join("summoner").join("config.toml"))
}

fn home_dir() -> Option<PathBuf> {
    home_dir_for(
        cfg!(windows),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

fn home_dir_for(
    windows: bool,
    home: Option<OsString>,
    user_profile: Option<OsString>,
) -> Option<PathBuf> {
    if windows {
        user_profile.or(home)
    } else {
        home.or(user_profile)
    }
    .map(PathBuf::from)
}

/// Parse one config file. A missing file is silent (config is optional); a file
/// that exists but cannot be read or parsed is warned about loudly and skipped —
/// dispatch settings must never silently revert to their defaults.
fn read(path: &Path) -> Option<Config> {
    let text = match read_text(path) {
        Ok(Some(text)) => text,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("summoner: cannot read config {}: {error}", path.display());
            return None;
        }
    };
    match toml::from_str(&text) {
        Ok(config) => Some(config),
        Err(error) => {
            eprintln!(
                "summoner: ignoring config {}: {}",
                path.display(),
                error.message()
            );
            None
        }
    }
}

fn read_text(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The nearest `.summoner.toml` at or above `cwd`, so a summoner invoked from a
/// repo subdirectory still reads that repo's config.
fn repo_config_path_from(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join(".summoner.toml"))
        .find(|path| path.exists())
}

fn merge(base: &mut Config, over: Config) {
    base.default_executor = over.default_executor.or(base.default_executor.take());
    base.max_parallel = over.max_parallel.or(base.max_parallel);
    base.default_verify_profile = over
        .default_verify_profile
        .or(base.default_verify_profile.take());
    base.order_timeout_secs = over.order_timeout_secs.or(base.order_timeout_secs);
    base.grove_bin = over.grove_bin.or(base.grove_bin.take());
    base.keep_failed_worktrees = over.keep_failed_worktrees.or(base.keep_failed_worktrees);
    // Per-name override: a repo redefining `codex` wins, while executors only
    // the global file defines stay available.
    base.executors.extend(over.executors);
}

pub fn load() -> Resolved {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    load_from(&cwd)
}

fn load_from(cwd: &Path) -> Resolved {
    let mut config = Config::default();
    let mut sources = Vec::new();
    if let Some(path) = global_path()
        && let Some(global) = read(&path)
    {
        merge(&mut config, global);
        sources.push(path.display().to_string());
    }
    if let Some(path) = repo_config_path_from(cwd)
        && let Some(repo) = read(&path)
    {
        merge(&mut config, repo);
        sources.push(path.display().to_string());
    }
    Resolved { sources, config }
}

/// Parse a boolean environment variable, accepting the common spellings. Unset
/// or unrecognized is `None`, so it falls through to the config or default.
fn env_bool(key: &str) -> Option<bool> {
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

    fn backend(argv: &[&str]) -> ExecutorBackend {
        ExecutorBackend {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            prompt: PromptRouting::Arg,
            timeout_secs: None,
            env_required: Vec::new(),
        }
    }

    #[test]
    fn repo_config_overrides_global_and_keeps_unset_globals() {
        let mut base = Config {
            default_executor: Some("glm".into()),
            max_parallel: Some(4),
            ..Config::default()
        };
        base.executors.insert("glm".into(), backend(&["opencode"]));
        base.executors.insert("codex".into(), backend(&["codex"]));

        let mut over = Config {
            max_parallel: Some(2),
            keep_failed_worktrees: Some(true),
            ..Config::default()
        };
        over.executors
            .insert("codex".into(), backend(&["codex", "exec"]));
        merge(&mut base, over);

        assert_eq!(base.max_parallel, Some(2));
        assert_eq!(base.keep_failed_worktrees, Some(true));
        // Global settings survive where the repo file is silent.
        assert_eq!(base.default_executor.as_deref(), Some("glm"));
        assert_eq!(base.executors["glm"].argv, ["opencode"]);
        // Same-name executor is replaced whole, not field-merged.
        assert_eq!(base.executors["codex"].argv, ["codex", "exec"]);
    }

    #[test]
    fn repo_config_is_found_from_a_subdirectory() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join(".summoner.toml"), "max_parallel = 7\n").unwrap();
        let deep = repo.path().join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();

        let found = repo_config_path_from(&deep).expect("ancestor walk finds the repo config");
        assert_eq!(read(&found).unwrap().max_parallel, Some(7));
    }

    #[test]
    fn unparseable_config_is_skipped_not_silently_defaulted_from() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".summoner.toml");
        std::fs::write(&path, "max_parallel = 2\nmax_paralel = 4\n").unwrap();
        // The typo'd file is rejected whole (deny_unknown_fields), never a
        // Config quietly missing the valid settings.
        assert!(read(&path).is_none());
    }

    #[test]
    fn executor_backend_parses_with_routing_and_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [executors.fake]
            argv = ["fake-agent", "{prompt}"]
            prompt = "stdin"
            env_required = ["FAKE_KEY"]
            "#,
        )
        .unwrap();
        let fake = &cfg.executors["fake"];
        assert_eq!(fake.prompt, PromptRouting::Stdin);
        assert_eq!(fake.env_required, ["FAKE_KEY"]);
        assert_eq!(fake.timeout_secs, None);

        let cfg: Config = toml::from_str(
            r#"
            [executors.plain]
            argv = ["plain"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.executors["plain"].prompt, PromptRouting::Arg);
    }

    #[test]
    fn defaults_apply_when_nothing_is_configured() {
        let cfg = Config::default();
        assert_eq!(cfg.max_parallel(), 2);
        assert_eq!(cfg.order_timeout_secs(), 600);
        assert_eq!(cfg.grove_bin(), "grove");
        assert!(!cfg.keep_failed_worktrees());
        assert_eq!(cfg.default_executor(), None);
    }

    #[test]
    fn home_resolution_uses_each_platforms_native_variable_first() {
        let home = Some(OsString::from("unix-home"));
        let profile = Some(OsString::from("windows-home"));
        assert_eq!(
            home_dir_for(false, home.clone(), profile.clone()),
            Some(PathBuf::from("unix-home"))
        );
        assert_eq!(
            home_dir_for(true, home, profile),
            Some(PathBuf::from("windows-home"))
        );
    }
}
