use super::GroveCli;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct InspectionAcquire {
    pub schema_version: u32,
    pub capsule_id: String,
    pub path: PathBuf,
    pub task_id: String,
    pub source_sha256: String,
}

#[derive(Deserialize)]
pub struct InspectionExec {
    pub schema_version: u32,
    pub capsule_id: String,
    pub task_id: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub tree_clean: bool,
    pub source_unchanged: bool,
    pub capsule_unchanged: bool,
    pub authorized: bool,
    pub source_sha256: String,
    pub stdout: InspectionLog,
    pub stderr: InspectionLog,
}

#[derive(Deserialize)]
pub struct InspectionLog {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

impl GroveCli {
    pub fn inspection_acquire(
        &self,
        worktree: &Path,
        task_id: &str,
        ttl_secs: u64,
    ) -> Result<InspectionAcquire> {
        let value = self.domain(
            worktree,
            &[
                "inspect",
                "acquire",
                "--task-id",
                task_id,
                "--ttl-secs",
                &ttl_secs.to_string(),
            ],
        )?;
        serde_json::from_value(value).context("parsing Grove inspection acquire report")
    }

    pub fn inspection_exec(
        &self,
        worktree: &Path,
        capsule_id: &str,
        timeout_secs: u64,
        command: &[String],
    ) -> Result<InspectionExec> {
        let timeout = timeout_secs.to_string();
        let mut args = vec![
            "inspect",
            "exec",
            capsule_id,
            "--timeout-secs",
            &timeout,
            "--",
        ];
        args.extend(command.iter().map(String::as_str));
        let value = self.domain(worktree, &args)?;
        serde_json::from_value(value).context("parsing Grove inspection execution report")
    }

    pub fn inspection_release(&self, worktree: &Path, capsule_id: &str) -> Result<()> {
        self.domain(worktree, &["inspect", "release", capsule_id])
            .map(|_| ())
    }
}
