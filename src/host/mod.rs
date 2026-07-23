//! Isolation and verification hosts. Summoner core talks only through [`Host`].
//!
//! - [`grove_host::GroveHost`] — current Grove CLI path (lanes, receipts, capsules).
//! - [`git_host::GitHost`] — default independence path: git worktrees + local ledger.

mod git_claim;
mod git_host;
mod git_ledger;
mod grove_host;
mod partition;
pub(crate) mod supervise;
mod types;
mod verify_config;

pub use git_host::GitHost;
pub use grove_host::GroveHost;
pub use types::{ExecutionPlan, HostCapabilities, HostInfo, TaskBeginRequest, WorktreeRequest};
pub use verify_config::VerificationConfig;

use crate::config::Config;
use crate::grove::{BeginOutcome, FinishOutcome, InspectionExec, ReleaseOutcome, VerifySummary};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Everything Summoner needs from an isolation/verification backend.
pub trait Host: Send + Sync {
    fn kind(&self) -> &str;
    fn preflight(&self) -> Result<HostInfo>;
    #[allow(dead_code)]
    fn capabilities(&self) -> HostCapabilities;

    fn worktree_acquire(&self, req: WorktreeRequest<'_>) -> Result<PathBuf>;
    fn worktree_release(&self, repo: &Path, worktree: &Path) -> Result<ReleaseOutcome>;

    fn task_begin(&self, req: TaskBeginRequest<'_>) -> Result<BeginOutcome>;
    fn execution_plan(
        &self,
        task_id: &str,
        timeout_secs: u64,
        executor: &[String],
    ) -> Result<ExecutionPlan>;
    fn task_finish(
        &self,
        worktree: &Path,
        task_id: &str,
        allow_unverified: Option<&str>,
        expected_source_sha256: Option<&str>,
    ) -> Result<FinishOutcome>;
    fn task_abandon(&self, worktree: &Path, task_id: &str, reason: &str) -> Result<()>;
    fn task_status(&self, repo: &Path) -> Result<serde_json::Value>;
    fn kill_supervised(&self, task_id: &str, worktree: &Path) -> Result<()>;

    fn verify(&self, worktree: &Path, profile: &str, task_id: &str) -> Result<VerifySummary>;
    fn partition(&self, repo: &Path, sets: &serde_json::Value) -> Result<serde_json::Value>;

    /// Grove inspection capsule; git host may return a weak local capsule.
    fn inspection_acquire(
        &self,
        worktree: &Path,
        task_id: &str,
        lease_secs: u64,
    ) -> Result<crate::grove::InspectionAcquire>;
    fn inspection_exec(
        &self,
        worktree: &Path,
        capsule_id: &str,
        timeout_secs: u64,
        argv: &[String],
    ) -> Result<InspectionExec>;
    fn inspection_release(&self, worktree: &Path, capsule_id: &str) -> Result<()>;

    /// Optional: generate Cargo.lock under host supervision (init demo).
    #[allow(dead_code)]
    fn cargo_generate_lockfile(&self, repo: &Path) -> Result<()> {
        let _ = repo;
        bail!("{} host cannot generate Cargo.lock", self.kind())
    }
}

/// Resolved host selection (also recorded in run manifests).
#[derive(Debug, Clone)]
pub struct ResolvedHost {
    pub kind: String,
    pub grove_bin: Option<String>,
    pub notice: Option<String>,
}

/// Resolve which host to use. Explicit config wins; then legacy `grove_bin`;
/// then `.grove.toml` + grove on PATH → grove (compat); else git.
pub fn resolve(config: &Config, repo: &Path) -> ResolvedHost {
    if let Some(host) = &config.host
        && let Some(kind) = host.kind.as_deref()
    {
        let kind = kind.to_ascii_lowercase();
        if kind == "grove" {
            return ResolvedHost {
                kind: "grove".into(),
                grove_bin: Some(
                    host.bin
                        .clone()
                        .or_else(|| config.grove_bin.clone())
                        .unwrap_or_else(|| "grove".into()),
                ),
                notice: None,
            };
        }
        if kind == "git" {
            return ResolvedHost {
                kind: "git".into(),
                grove_bin: None,
                notice: None,
            };
        }
    }
    // Config field or SUMMONER_GROVE_BIN both select the grove host (env is
    // how tests and CI pin an exact Grove binary without writing config).
    if config.grove_bin.is_some() || std::env::var_os("SUMMONER_GROVE_BIN").is_some() {
        return ResolvedHost {
            kind: "grove".into(),
            grove_bin: Some(config.grove_bin()),
            notice: Some(
                "summoner: grove_bin / SUMMONER_GROVE_BIN selects the grove host; set [host] kind explicitly"
                    .into(),
            ),
        };
    }
    let grove_toml = repo.join(".grove.toml").is_file();
    let grove_on_path = which("grove");
    if grove_toml && grove_on_path {
        return ResolvedHost {
            kind: "grove".into(),
            grove_bin: Some("grove".into()),
            notice: Some(
                "summoner: .grove.toml + grove on PATH → grove host (set [host] kind = \"git\" to force independence)"
                    .into(),
            ),
        };
    }
    ResolvedHost {
        kind: "git".into(),
        grove_bin: None,
        notice: None,
    }
}

pub fn open(config: &Config, repo: &Path) -> Result<Arc<dyn Host>> {
    let resolved = resolve(config, repo);
    if let Some(notice) = &resolved.notice {
        eprintln!("{notice}");
    }
    match resolved.kind.as_str() {
        "grove" => {
            let bin = resolved
                .grove_bin
                .clone()
                .unwrap_or_else(|| config.grove_bin());
            Ok(Arc::new(GroveHost::new(bin)))
        }
        "git" => {
            let root = config
                .host
                .as_ref()
                .and_then(|h| h.worktree_root.clone())
                .map(PathBuf::from);
            let verify = verify_config::load(config);
            Ok(Arc::new(GitHost::new(repo, root, verify)?))
        }
        other => bail!("unknown host kind {other:?}"),
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(bin);
                candidate.is_file() || {
                    #[cfg(windows)]
                    {
                        dir.join(format!("{bin}.exe")).is_file()
                            || dir.join(format!("{bin}.cmd")).is_file()
                    }
                    #[cfg(not(windows))]
                    {
                        false
                    }
                }
            })
        })
        .unwrap_or(false)
}

/// Compatibility: build a Grove host the way pre-Host code did.
#[allow(dead_code)]
pub fn grove_only(bin: String) -> Arc<dyn Host> {
    Arc::new(GroveHost::new(bin))
}
