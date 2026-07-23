//! Shared host outcomes and execution plans.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How the host wants an executor/reviewer process launched.
#[derive(Debug, Clone)]
pub enum ExecutionPlan {
    /// Host wraps the argv (e.g. `grove task exec --timeout-secs … --`).
    HostWrapped {
        argv: Vec<String>,
        /// Seconds after which Summoner may backup-kill if the host wedges.
        backup_grace_secs: u64,
    },
    /// Summoner owns the hard deadline and process-group / Job Object kill.
    SummonerSupervised {
        argv: Vec<String>,
        timeout_secs: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub kind: String,
    pub version: String,
    pub state_root: Option<PathBuf>,
    pub capabilities: HostCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCapabilities {
    pub supervised_exec: bool,
    pub receipt_finish: bool,
    pub cargo_topology: bool,
    pub cow_lanes: bool,
    pub inspection_capsule: bool,
    pub protected_paths: Vec<String>,
}

impl Default for HostCapabilities {
    fn default() -> Self {
        Self {
            supervised_exec: false,
            receipt_finish: false,
            cargo_topology: false,
            cow_lanes: false,
            inspection_capsule: false,
            protected_paths: vec![".summoner.toml".into()],
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorktreeRequest<'a> {
    pub repo: &'a std::path::Path,
    pub run_id: &'a str,
    pub order_id: &'a str,
    pub attempt: u64,
    pub agent: &'a str,
    pub branch: Option<&'a str>,
    pub base: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct TaskBeginRequest<'a> {
    pub worktree: &'a std::path::Path,
    pub run_id: &'a str,
    pub order_id: &'a str,
    pub attempt: u64,
    pub agent: &'a str,
    pub title: &'a str,
    pub scope: &'a [String],
    pub claim_group: Option<&'a str>,
}

/// Normalized task status for resume (hosts map their records into this).
#[allow(dead_code)] // reserved for resume mapping; not yet read by callers
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostTaskView {
    pub id: String,
    pub lifecycle: String,
    pub verification: String,
    pub branch: Option<String>,
    pub worktree: Option<String>,
}
