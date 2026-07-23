//! Grove CLI host: wraps existing [`crate::grove::GroveCli`].

use super::Host;
use super::types::{ExecutionPlan, HostCapabilities, HostInfo, TaskBeginRequest, WorktreeRequest};
use crate::grove::{
    BeginOutcome, FinishOutcome, GroveCli, InspectionAcquire, InspectionExec, ReleaseOutcome,
    VerifySummary,
};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct GroveHost {
    inner: GroveCli,
    bin: String,
}

impl GroveHost {
    pub fn new(bin: String) -> Self {
        Self {
            inner: GroveCli::new(bin.clone()),
            bin,
        }
    }

    #[allow(dead_code)]
    pub fn cli(&self) -> &GroveCli {
        &self.inner
    }
}

impl Host for GroveHost {
    fn kind(&self) -> &str {
        "grove"
    }

    fn preflight(&self) -> Result<HostInfo> {
        let _caps = self.inner.preflight()?;
        Ok(HostInfo {
            kind: "grove".into(),
            version: self.inner.version().unwrap_or_else(|_| self.bin.clone()),
            state_root: None,
            capabilities: self.capabilities(),
        })
    }

    fn capabilities(&self) -> HostCapabilities {
        HostCapabilities {
            supervised_exec: true,
            receipt_finish: true,
            cargo_topology: true,
            cow_lanes: true,
            inspection_capsule: true,
            protected_paths: vec![
                ".summoner.toml".into(),
                ".grove.toml".into(),
                "rust-toolchain".into(),
                "rust-toolchain.toml".into(),
                ".cargo/config".into(),
                ".cargo/config.toml".into(),
            ],
        }
    }

    fn worktree_acquire(&self, req: WorktreeRequest<'_>) -> Result<PathBuf> {
        self.inner
            .worktree_acquire(req.repo, req.agent, req.branch, req.base)
    }

    fn worktree_release(&self, repo: &Path, worktree: &Path) -> Result<ReleaseOutcome> {
        self.inner.worktree_release(repo, worktree)
    }

    fn task_begin(&self, req: TaskBeginRequest<'_>) -> Result<BeginOutcome> {
        self.inner.task_begin(
            req.worktree,
            req.agent,
            req.title,
            req.scope,
            req.claim_group,
        )
    }

    fn execution_plan(
        &self,
        task_id: &str,
        timeout_secs: u64,
        executor: &[String],
    ) -> Result<ExecutionPlan> {
        Ok(ExecutionPlan::HostWrapped {
            argv: self.inner.exec_argv(task_id, timeout_secs, executor),
            backup_grace_secs: 30,
        })
    }

    fn task_finish(
        &self,
        worktree: &Path,
        task_id: &str,
        allow_unverified: Option<&str>,
        expected_source_sha256: Option<&str>,
    ) -> Result<FinishOutcome> {
        self.inner
            .task_finish(worktree, task_id, allow_unverified, expected_source_sha256)
    }

    fn task_abandon(&self, worktree: &Path, task_id: &str, reason: &str) -> Result<()> {
        self.inner.task_abandon(worktree, task_id, reason)
    }

    fn task_status(&self, repo: &Path) -> Result<serde_json::Value> {
        self.inner.task_status(repo)
    }

    fn kill_supervised(&self, task_id: &str, worktree: &Path) -> Result<()> {
        // Best-effort: task status may expose a supervisor pid; outcome module
        // already implements group kill from Grove records when available.
        let _ = (task_id, worktree);
        Ok(())
    }

    fn verify(&self, worktree: &Path, profile: &str, task_id: &str) -> Result<VerifySummary> {
        self.inner.verify(worktree, profile, task_id)
    }

    fn partition(&self, repo: &Path, sets: &serde_json::Value) -> Result<serde_json::Value> {
        self.inner.partition(repo, sets)
    }

    fn inspection_acquire(
        &self,
        worktree: &Path,
        task_id: &str,
        lease_secs: u64,
    ) -> Result<InspectionAcquire> {
        self.inner.inspection_acquire(worktree, task_id, lease_secs)
    }

    fn inspection_exec(
        &self,
        worktree: &Path,
        capsule_id: &str,
        timeout_secs: u64,
        argv: &[String],
    ) -> Result<InspectionExec> {
        self.inner
            .inspection_exec(worktree, capsule_id, timeout_secs, argv)
    }

    fn inspection_release(&self, worktree: &Path, capsule_id: &str) -> Result<()> {
        self.inner.inspection_release(worktree, capsule_id)
    }

    fn cargo_generate_lockfile(&self, repo: &Path) -> Result<()> {
        self.inner.cargo_generate_lockfile(repo)
    }
}
