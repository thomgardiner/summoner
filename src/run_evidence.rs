//! Immutable run inputs and write-once JSON snapshots.

use crate::config::Config;
use crate::order::Order;
use crate::run_manifest::{
    Backend, ExpandedOrder, Manifest, ManifestOrder, Roles, SCHEMA_VERSION, Settings,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_manifest(
    dir: &Path,
    run_id: &str,
    repo: &Path,
    selected_profile: Option<&str>,
    grove_version: &str,
    config: &Config,
    orders: &[Order],
) -> Result<Config> {
    let manifest = manifest(
        run_id,
        repo,
        selected_profile,
        grove_version,
        config,
        orders,
    )?;
    let text = serde_json::to_vec_pretty(&manifest).context("serializing manifest.json")?;
    reject_values(&text, &manifest.backends)?;
    let bound = crate::run_manifest::bound_config(&manifest, config)?;
    write_new(&dir.join("manifest.json"), &text)?;
    Ok(bound)
}

fn manifest(
    run_id: &str,
    repo: &Path,
    selected_profile: Option<&str>,
    grove_version: &str,
    config: &Config,
    orders: &[Order],
) -> Result<Manifest> {
    let mut names = BTreeSet::new();
    for order in orders {
        names.extend(
            [order.executor_name(config), order.reviewer_name(config)]
                .into_iter()
                .flatten(),
        );
    }
    let backends = names
        .into_iter()
        .map(|name| {
            let backend = config.executors.get(&name).expect("validated backend");
            Ok((name, Backend::capture(backend, repo)?))
        })
        .collect::<Result<_>>()?;
    let orders = orders
        .iter()
        .map(|order| {
            Ok(ManifestOrder {
                source_path: order.source.display().to_string(),
                source_text: std::fs::read_to_string(&order.source).with_context(|| {
                    format!("reading order snapshot {}", order.source.display())
                })?,
                expanded: ExpandedOrder::from(order),
                roles: Some(Roles {
                    executor: order
                        .executor_name(config)
                        .expect("validated executor role"),
                    reviewer: order.reviewer_name(config),
                }),
            })
        })
        .collect::<Result<_>>()?;
    Ok(Manifest {
        schema_version: SCHEMA_VERSION,
        run_id: run_id.to_string(),
        repository: repo.display().to_string(),
        start_head: git_head(repo)?,
        selected_profile: selected_profile.map(String::from),
        summoner_version: env!("CARGO_PKG_VERSION").to_string(),
        grove_version: grove_version.to_string(),
        settings: Settings {
            max_parallel: config.max_parallel(),
            default_verify_profile: config.default_verify_profile.clone(),
            order_timeout_secs: config.order_timeout_secs(),
            keep_failed_worktrees: config.keep_failed_worktrees(),
            fail_fast: config.fail_fast(),
            revise: config.revise(),
            run_token_budget: config.run_token_budget(),
            trusted_policy: config.trusted_policy.clone(),
            trusted_policy_sha256: config.trusted_policy.as_ref().map(|policy| policy.sha256()),
        },
        orders,
        backends,
    })
}

fn git_head(repo: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .context("reading repository start HEAD")?;
    if !output.status.success() {
        bail!("reading repository start HEAD failed")
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
fn reject_values(text: &[u8], backends: &BTreeMap<String, Backend>) -> Result<()> {
    for variable in backends.values().flat_map(|backend| &backend.env_required) {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            let escaped = serde_json::to_string(&value)?;
            if text
                .windows(escaped.trim_matches('"').len())
                .any(|part| part == escaped.trim_matches('"').as_bytes())
            {
                bail!(
                    "manifest evidence contains value for required environment variable {variable}"
                )
            }
        }
    }
    Ok(())
}
pub(crate) fn write_once(path: &Path, value: &impl Serialize) -> Result<()> {
    let text = serde_json::to_vec_pretty(value).context("serializing JSON")?;
    write_new(path, &text)
}

pub(crate) fn write_exact(path: &Path, text: &[u8]) -> Result<()> {
    if path.exists() {
        let existing =
            std::fs::read(path).with_context(|| format!("reading existing {}", path.display()))?;
        if existing == text {
            return Ok(());
        }
        bail!("refusing to replace mismatched {}", path.display());
    }
    write_new(path, text)
}

fn write_new(path: &Path, text: &[u8]) -> Result<()> {
    if path.exists() {
        bail!("refusing to replace existing {}", path.display());
    }
    let temp = temporary_path(path);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    let result = (|| -> Result<()> {
        let mut writer = BufWriter::new(file);
        writer.write_all(text).context("writing immutable file")?;
        writer.flush().context("flushing immutable file")?;
        writer
            .get_ref()
            .sync_all()
            .context("syncing immutable file")?;
        std::fs::hard_link(&temp, path).with_context(|| format!("creating {}", path.display()))?;
        std::fs::remove_file(&temp).with_context(|| format!("removing {}", temp.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
fn temporary_path(path: &Path) -> PathBuf {
    let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        id
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExecutorBackend, PromptRouting};

    #[test]
    fn complete_file_is_written_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        write_once(&path, &serde_json::json!({"schema_version": 1})).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"schema_version\": 1\n}"
        );
        assert!(write_once(&path, &serde_json::json!({})).is_err());
    }

    struct Fails;
    impl Serialize for Fails {
        fn serialize<S>(&self, _: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("nope"))
        }
    }

    #[test]
    fn serialization_failure_leaves_no_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.json");
        assert!(write_once(&path, &Fails).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn required_value_is_refused_without_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let var = "SUMMONER_RUN_EVIDENCE_TEST_SECRET";
        let value = "manifest-\"secret\"\\value\n";
        unsafe { std::env::set_var(var, value) };
        let source = dir.path().join("order.toml");
        std::fs::write(&source, value).unwrap();
        let mut config = Config {
            default_executor: Some("test".into()),
            ..Config::default()
        };
        config.executors.insert(
            "test".into(),
            ExecutorBackend {
                argv: vec![std::env::current_exe().unwrap().display().to_string()],
                prompt: None,
                timeout_secs: None,
                env_required: vec![var.into()],
                usage_marker: None,
                session_marker: None,
                resume_argv: vec![],
                provenance: None,
                resume_provenance: None,
            },
        );
        let order = Order {
            id: "test".into(),
            title: "test".into(),
            brief: "test".into(),
            scope: vec![],
            acceptance: vec![],
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            max_tokens: None,
            base: None,
            branch: None,
            after: vec![],
            variants: vec![],
            claim_group: None,
            variant_of: None,
            source,
        };
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let error = write_manifest(dir.path(), "run", repo, None, "grove", &config, &[order])
            .err()
            .expect("secret must reject the manifest")
            .to_string();
        unsafe { std::env::remove_var(var) };
        assert!(error.contains(var));
        assert!(!error.contains(value));
        assert!(!dir.path().join("manifest.json").exists());
    }

    #[test]
    fn manifest_records_backend_executable_provenance() {
        let manifest = Manifest {
            schema_version: 3,
            run_id: "run-1".into(),
            repository: "/repo".into(),
            start_head: "abc".into(),
            selected_profile: Some("codex".into()),
            summoner_version: "0.1.0".into(),
            grove_version: "grove 0.3.2".into(),
            settings: Settings {
                max_parallel: 2,
                default_verify_profile: None,
                order_timeout_secs: 600,
                keep_failed_worktrees: false,
                fail_fast: None,
                revise: 0,
                run_token_budget: None,
                trusted_policy: None,
                trusted_policy_sha256: None,
            },
            orders: vec![ManifestOrder {
                source_path: "/repo/a.toml".into(),
                source_text: "id = \\\"a\\\"\\n".into(),
                expanded: ExpandedOrder {
                    id: "a".into(),
                    title: "A".into(),
                    brief: "B".into(),
                    scope: vec!["src".into()],
                    acceptance: vec![],
                    verify_profile: None,
                    executor: Some("codex".into()),
                    reviewer: None,
                    timeout_secs: None,
                    max_tokens: None,
                    base: None,
                    branch: None,
                    after: vec![],
                    claim_group: None,
                    variant_of: None,
                },
                roles: Some(Roles {
                    executor: "codex".into(),
                    reviewer: None,
                }),
            }],
            backends: BTreeMap::from([(
                "codex".into(),
                Backend {
                    argv: vec!["codex".into()],
                    resume_argv: vec![],
                    prompt: PromptRouting::Arg,
                    timeout_secs: None,
                    usage_marker: None,
                    session_marker: None,
                    env_required: vec!["TOKEN".into()],
                    provenance: crate::backend_provenance::capture(
                        std::env::current_exe().unwrap().to_str().unwrap(),
                        Path::new(env!("CARGO_MANIFEST_DIR")),
                    )
                    .unwrap(),
                    resume_provenance: None,
                },
            )]),
        };
        let value = serde_json::to_value(&manifest).unwrap();
        assert_eq!(value["schema_version"], 3);
        let provenance = &value["backends"]["codex"]["provenance"];
        assert!(
            provenance["resolved_path"]
                .as_str()
                .unwrap()
                .contains("summoner")
        );
        assert_eq!(provenance["binary_sha256"].as_str().unwrap().len(), 64);
        assert!(provenance["version_output"].is_string());
    }
}
