//! Versioned run manifest schema and immutable replay.

use crate::config::{Config, ExecutorBackend, PromptRouting};
use crate::order::Order;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) schema_version: u32,
    pub(crate) run_id: String,
    pub(crate) repository: String,
    pub(crate) start_head: String,
    pub(crate) selected_profile: Option<String>,
    pub(crate) summoner_version: String,
    pub(crate) grove_version: String,
    pub(crate) settings: Settings,
    pub(crate) orders: Vec<ManifestOrder>,
    pub(crate) backends: BTreeMap<String, Backend>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Settings {
    pub(crate) max_parallel: usize,
    pub(crate) default_verify_profile: Option<String>,
    pub(crate) order_timeout_secs: u64,
    pub(crate) keep_failed_worktrees: bool,
    pub(crate) fail_fast: Option<usize>,
    pub(crate) revise: usize,
    pub(crate) run_token_budget: Option<u64>,
    /// The trusted policy in force, recorded verbatim with its content
    /// address: a resumed run is gated by the same bar the original ran under.
    #[serde(default)]
    pub(crate) trusted_policy: Option<crate::config::TrustedPolicy>,
    #[serde(default)]
    pub(crate) trusted_policy_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ManifestOrder {
    pub(crate) source_path: String,
    pub(crate) source_text: String,
    pub(crate) expanded: ExpandedOrder,
    pub(crate) roles: Option<Roles>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Roles {
    pub(crate) executor: String,
    pub(crate) reviewer: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ExpandedOrder {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) brief: String,
    pub(crate) scope: Vec<String>,
    pub(crate) acceptance: Vec<String>,
    pub(crate) verify_profile: Option<String>,
    pub(crate) executor: Option<String>,
    pub(crate) reviewer: Option<String>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) max_tokens: Option<u64>,
    pub(crate) base: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) after: Vec<String>,
    pub(crate) claim_group: Option<String>,
    pub(crate) variant_of: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Backend {
    pub(crate) argv: Vec<String>,
    pub(crate) resume_argv: Vec<String>,
    pub(crate) prompt: PromptRouting,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) usage_marker: Option<String>,
    pub(crate) session_marker: Option<String>,
    pub(crate) env_required: Vec<String>,
    pub(crate) provenance: crate::backend_provenance::Provenance,
    pub(crate) resume_provenance: Option<crate::backend_provenance::Provenance>,
}

pub(crate) struct Replay {
    pub(crate) config: Config,
    pub(crate) orders: Vec<Order>,
    pub(crate) selected_profile: Option<String>,
}

pub(crate) fn replay(dir: &Path, run_id: &str, current: &Config) -> Result<Replay> {
    let path = dir.join("manifest.json");
    let manifest: Manifest = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing immutable run manifest")?;
    validate(&manifest, run_id)?;
    let config = bound_config(&manifest, current)?;
    let orders = orders(dir, &manifest)?;
    let orders = crate::run_prepare::checked(orders, &config)?;
    Ok(Replay {
        config,
        orders,
        selected_profile: manifest.selected_profile,
    })
}

fn validate(manifest: &Manifest, run_id: &str) -> Result<()> {
    if manifest.schema_version != SCHEMA_VERSION {
        bail!(
            "run manifest schema mismatch: need {}, found {}",
            SCHEMA_VERSION,
            manifest.schema_version
        );
    }
    if manifest.run_id != run_id {
        bail!(
            "run manifest id mismatch: requested {run_id:?}, found {:?}",
            manifest.run_id
        );
    }
    let current = std::env::current_dir()
        .context("resolving current repository")?
        .canonicalize()
        .context("canonicalizing current repository")?;
    let recorded = PathBuf::from(&manifest.repository)
        .canonicalize()
        .context("canonicalizing the manifest repository")?;
    if current != recorded {
        bail!(
            "run {run_id} belongs to {}, not {}",
            recorded.display(),
            current.display()
        );
    }
    Ok(())
}

pub(crate) fn bound_config(manifest: &Manifest, current: &Config) -> Result<Config> {
    let repo = Path::new(&manifest.repository);
    for (name, backend) in &manifest.backends {
        let binary = backend
            .argv
            .first()
            .ok_or_else(|| anyhow!("run manifest backend {name:?} has an empty argv"))?;
        crate::backend_provenance::require_current(&backend.provenance, binary, repo)
            .with_context(|| format!("validating recorded backend {name:?}"))?;
        if let Some(expected) = &backend.resume_provenance {
            let resume = backend.resume_argv.first().ok_or_else(|| {
                anyhow!("run manifest backend {name:?} has resume provenance but no resume argv")
            })?;
            crate::backend_provenance::require_current(expected, resume, repo)
                .with_context(|| format!("validating recorded resume backend {name:?}"))?;
        }
    }
    let mut config = Config {
        default_executor: current.default_executor(),
        default_reviewer: current.default_reviewer(),
        max_parallel: Some(manifest.settings.max_parallel),
        default_verify_profile: manifest.settings.default_verify_profile.clone(),
        order_timeout_secs: Some(manifest.settings.order_timeout_secs),
        grove_bin: Some(current.grove_bin()),
        keep_failed_worktrees: Some(manifest.settings.keep_failed_worktrees),
        fail_fast: manifest.settings.fail_fast,
        revise: Some(manifest.settings.revise),
        run_token_budget: manifest.settings.run_token_budget,
        allow_unknown_auth: current.allow_unknown_auth.clone(),
        // The recorded policy, not today's config: a resume is gated by the bar
        // the run started under, and its digest must still match.
        trusted_policy: manifest.settings.trusted_policy.clone(),
        ..Config::default()
    };
    if let Some(policy) = config.trusted_policy.as_ref() {
        let recorded = manifest
            .settings
            .trusted_policy_sha256
            .as_deref()
            .context("run manifest records a trusted policy without its digest")?;
        let actual = policy.sha256();
        if actual != recorded {
            bail!("recorded trusted policy does not match its digest: {recorded} vs {actual}");
        }
    }
    config.executors = manifest
        .backends
        .iter()
        .map(|(name, backend)| (name.clone(), ExecutorBackend::from(backend)))
        .collect();
    config.freeze();
    if config.executors.is_empty() {
        bail!("run manifest contains no selected executor backends");
    }
    Ok(config)
}

fn orders(dir: &Path, manifest: &Manifest) -> Result<Vec<Order>> {
    let root = dir.join("resume-orders");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating replay order directory {}", root.display()))?;
    manifest
        .orders
        .iter()
        .map(|record| order(&root, &manifest.start_head, record))
        .collect()
}

fn order(root: &Path, start_head: &str, record: &ManifestOrder) -> Result<Order> {
    let roles = record.roles.as_ref().ok_or_else(|| {
        anyhow!(
            "run manifest order {:?} lacks resolved executor/reviewer roles; start a new run",
            record.expanded.id
        )
    })?;
    if !record.source_path.ends_with(".toml") && !record.source_path.ends_with(".json") {
        bail!(
            "run manifest order {:?} has unsupported source path {:?}",
            record.expanded.id,
            record.source_path
        );
    }
    let suffix = if record.source_path.ends_with(".json") {
        "json"
    } else {
        "toml"
    };
    let source = root.join(format!("{}.{}", record.expanded.id, suffix));
    crate::run_evidence::write_exact(&source, record.source_text.as_bytes())?;
    let expanded = &record.expanded;
    Ok(Order {
        id: expanded.id.clone(),
        title: expanded.title.clone(),
        brief: expanded.brief.clone(),
        scope: expanded.scope.clone(),
        acceptance: expanded.acceptance.clone(),
        verify_profile: expanded.verify_profile.clone(),
        executor: Some(roles.executor.clone()),
        reviewer: Some(roles.reviewer.clone().unwrap_or_else(|| "none".into())),
        timeout_secs: expanded.timeout_secs,
        max_tokens: expanded.max_tokens,
        // Pinning the recorded start keeps a resumed order reproducible when
        // the default branch has moved — but only for orders with no
        // dependencies. An `after` order derives its base from its
        // dependencies' candidate commits at dispatch, and an explicit base
        // outranks that derivation, so synthesizing one here would silently
        // disconnect a resumed dependent from the work it depends on.
        base: expanded.base.clone().or_else(|| {
            (expanded.branch.is_none() && expanded.after.is_empty()).then(|| start_head.to_string())
        }),
        branch: expanded.branch.clone(),
        after: expanded.after.clone(),
        variants: Vec::new(),
        claim_group: expanded.claim_group.clone(),
        variant_of: expanded.variant_of.clone(),
        source,
    })
}

impl From<&Order> for ExpandedOrder {
    fn from(order: &Order) -> Self {
        Self {
            id: order.id.clone(),
            title: order.title.clone(),
            brief: order.brief.clone(),
            scope: order.scope.clone(),
            acceptance: order.acceptance.clone(),
            verify_profile: order.verify_profile.clone(),
            executor: order.executor.clone(),
            reviewer: order.reviewer.clone(),
            timeout_secs: order.timeout_secs,
            max_tokens: order.max_tokens,
            base: order.base.clone(),
            branch: order.branch.clone(),
            after: order.after.clone(),
            claim_group: order.claim_group.clone(),
            variant_of: order.variant_of.clone(),
        }
    }
}

impl Backend {
    pub(crate) fn capture(backend: &ExecutorBackend, repo: &Path) -> Result<Self> {
        let binary = backend
            .argv
            .first()
            .context("validated executor has an empty argv")?;
        let provenance = crate::backend_provenance::capture(binary, repo)?;
        let resume_provenance = backend
            .resume_argv
            .first()
            .map(|binary| crate::backend_provenance::capture(binary, repo))
            .transpose()?;
        let mut argv = backend.argv.clone();
        argv[0] = provenance.resolved_path.clone();
        let mut resume_argv = backend.resume_argv.clone();
        if let (Some(binary), Some(expected)) = (resume_argv.first_mut(), &resume_provenance) {
            *binary = expected.resolved_path.clone();
        }
        Ok(Self {
            argv,
            resume_argv,
            prompt: backend.routing(),
            timeout_secs: backend.timeout_secs,
            usage_marker: backend.usage_marker.clone(),
            session_marker: backend.session_marker.clone(),
            env_required: backend.env_required.clone(),
            provenance,
            resume_provenance,
        })
    }
}

impl From<&Backend> for ExecutorBackend {
    fn from(backend: &Backend) -> Self {
        Self {
            argv: backend.argv.clone(),
            prompt: Some(backend.prompt),
            timeout_secs: backend.timeout_secs,
            env_required: backend.env_required.clone(),
            usage_marker: backend.usage_marker.clone(),
            session_marker: backend.session_marker.clone(),
            resume_argv: backend.resume_argv.clone(),
            provenance: Some(backend.provenance.clone()),
            resume_provenance: backend.resume_provenance.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_backend_launches_the_canonical_recorded_binary() {
        let executable = std::env::current_exe().unwrap();
        let backend = ExecutorBackend {
            argv: vec![executable.display().to_string(), "run".into()],
            prompt: None,
            timeout_secs: None,
            env_required: vec![],
            usage_marker: None,
            session_marker: None,
            resume_argv: vec![executable.display().to_string(), "resume".into()],
            provenance: None,
            resume_provenance: None,
        };
        let captured = Backend::capture(&backend, Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let bound = ExecutorBackend::from(&captured);
        assert_eq!(bound.argv[0], captured.provenance.resolved_path);
        assert_eq!(
            bound.resume_argv[0],
            captured.resume_provenance.as_ref().unwrap().resolved_path
        );
        assert_eq!(bound.provenance, Some(captured.provenance));
        assert_eq!(bound.resume_provenance, captured.resume_provenance);
    }

    fn replay_record(id: &str, after: &[&str]) -> ManifestOrder {
        ManifestOrder {
            source_path: format!("/repo/{id}.toml"),
            source_text: format!("id = \"{id}\"\n"),
            expanded: ExpandedOrder {
                id: id.into(),
                title: "t".into(),
                brief: "b".into(),
                scope: vec!["src".into()],
                acceptance: vec![],
                verify_profile: None,
                executor: None,
                reviewer: None,
                timeout_secs: None,
                max_tokens: None,
                base: None,
                branch: None,
                after: after.iter().map(|s| s.to_string()).collect(),
                claim_group: None,
                variant_of: None,
            },
            roles: Some(Roles {
                executor: "fake".into(),
                reviewer: None,
            }),
        }
    }

    /// The resume regression from the round-four review: replay pins the run's
    /// start commit as the base of independent orders (reproducibility when
    /// the default branch has moved), but must NEVER synthesize one for an
    /// `after` order — an explicit base outranks dependency inheritance, so a
    /// synthesized base would silently disconnect a resumed dependent from the
    /// work it depends on.
    #[test]
    fn replay_pins_start_head_only_for_orders_without_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let independent = order(dir.path(), "abc123", &replay_record("solo", &[])).unwrap();
        assert_eq!(independent.base.as_deref(), Some("abc123"));

        let dependent = order(dir.path(), "abc123", &replay_record("child", &["solo"])).unwrap();
        assert_eq!(
            dependent.base, None,
            "a resumed dependent must derive its base from its dependencies"
        );
        assert_eq!(dependent.after, vec!["solo".to_string()]);
    }
}
