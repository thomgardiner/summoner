//! Durable task ledger for the git host (resume agreement).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Begun,
    Executing,
    Verifying,
    Finished,
    Abandoned,
    Refused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema_version: u32,
    pub id: String,
    pub run_id: String,
    pub order_id: String,
    pub attempt: u64,
    pub agent: String,
    pub title: String,
    pub scope: Vec<String>,
    pub claim_group: Option<String>,
    pub branch: Option<String>,
    pub worktree: String,
    /// Immutable HEAD at task begin; scope and finish compare against this.
    pub start_commit: String,
    /// HEAD (or bound source digest) when each required profile last passed.
    #[serde(default)]
    pub verify_source_commit: Option<String>,
    /// Content digest of the candidate when verification last fully passed.
    #[serde(default)]
    pub verify_source_sha256: Option<String>,
    pub state: TaskState,
    pub verification: crate::grove::TaskVerification,
    pub owner_pid: u32,
    pub updated_at: u64,
}

pub struct Ledger {
    dir: PathBuf,
}

impl Ledger {
    pub fn open(state_root: &Path) -> Result<Self> {
        let dir = state_root.join("tasks");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn write(&self, record: &TaskRecord) -> Result<()> {
        let path = self.path(&record.id);
        let temp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(record)?;
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    pub fn read(&self, id: &str) -> Result<TaskRecord> {
        let path = self.path(id);
        let bytes =
            std::fs::read(&path).with_context(|| format!("reading task {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn list(&self) -> Result<Vec<TaskRecord>> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(out);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path)
                && let Ok(rec) = serde_json::from_slice::<TaskRecord>(&bytes)
            {
                out.push(rec);
            }
        }
        Ok(out)
    }

    pub fn set_state(&self, id: &str, state: TaskState) -> Result<TaskRecord> {
        let mut rec = self.read(id)?;
        rec.state = state;
        rec.updated_at = now_secs();
        self.write(&rec)?;
        Ok(rec)
    }

    pub fn set_verification(
        &self,
        id: &str,
        verification: crate::grove::TaskVerification,
        state: TaskState,
    ) -> Result<TaskRecord> {
        let mut rec = self.read(id)?;
        rec.verification = verification;
        rec.state = state;
        rec.updated_at = now_secs();
        self.write(&rec)?;
        Ok(rec)
    }
}

pub fn new_task_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("git-{nanos:x}-{:x}", std::process::id())
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn require_active(rec: &TaskRecord) -> Result<()> {
    match rec.state {
        TaskState::Finished | TaskState::Abandoned | TaskState::Refused => {
            bail!("task {} is already terminal ({:?})", rec.id, rec.state)
        }
        _ => Ok(()),
    }
}
