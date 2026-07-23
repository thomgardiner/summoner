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

/// What a host actually guarantees. Callers must not treat two hosts with
/// different exact-state flags as interchangeable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostCapabilities {
    pub supervised_exec: bool,
    pub receipt_finish: bool,
    pub cargo_topology: bool,
    pub cow_lanes: bool,
    /// Private/immutable review capsule (not the live worktree).
    pub inspection_capsule: bool,
    /// Scope enforcement sees committed deltas since task begin, not only dirty tree.
    pub scope_includes_committed_delta: bool,
    /// Passed verification profiles are bound to a candidate source digest.
    pub verification_bound_to_source: bool,
    /// Review runs against an immutable snapshot, not a mutable live tree.
    pub immutable_inspection_snapshot: bool,
    /// Reviewer process cannot amend the candidate under review.
    pub review_process_isolated: bool,
    /// Finish refuses when the candidate no longer matches the bound digest.
    pub finish_source_compare_and_swap: bool,
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
            scope_includes_committed_delta: false,
            verification_bound_to_source: false,
            immutable_inspection_snapshot: false,
            review_process_isolated: false,
            finish_source_compare_and_swap: false,
            protected_paths: vec![".summoner.toml".into()],
        }
    }
}

impl HostCapabilities {
    /// Exact-state bar used by trusted policy and reviewed runs.
    pub fn supports_exact_state(&self) -> bool {
        self.scope_includes_committed_delta
            && self.verification_bound_to_source
            && self.finish_source_compare_and_swap
    }

    /// Review that claims independent judgment of an immutable candidate.
    pub fn supports_held_review(&self) -> bool {
        self.supports_exact_state()
            && self.immutable_inspection_snapshot
            && self.review_process_isolated
    }

    pub fn missing_for_held_review(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.scope_includes_committed_delta {
            missing.push("scope_includes_committed_delta");
        }
        if !self.verification_bound_to_source {
            missing.push("verification_bound_to_source");
        }
        if !self.finish_source_compare_and_swap {
            missing.push("finish_source_compare_and_swap");
        }
        if !self.immutable_inspection_snapshot {
            missing.push("immutable_inspection_snapshot");
        }
        if !self.review_process_isolated {
            missing.push("review_process_isolated");
        }
        missing
    }
}

/// Minimum host properties a trusted policy may demand.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RequiredHostCapabilities {
    pub scope_includes_committed_delta: bool,
    pub verification_bound_to_source: bool,
    pub immutable_inspection_snapshot: bool,
    pub review_process_isolated: bool,
    pub finish_source_compare_and_swap: bool,
}

impl RequiredHostCapabilities {
    pub fn unsatisfied_by(&self, caps: &HostCapabilities) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.scope_includes_committed_delta && !caps.scope_includes_committed_delta {
            missing.push("scope_includes_committed_delta");
        }
        if self.verification_bound_to_source && !caps.verification_bound_to_source {
            missing.push("verification_bound_to_source");
        }
        if self.immutable_inspection_snapshot && !caps.immutable_inspection_snapshot {
            missing.push("immutable_inspection_snapshot");
        }
        if self.review_process_isolated && !caps.review_process_isolated {
            missing.push("review_process_isolated");
        }
        if self.finish_source_compare_and_swap && !caps.finish_source_compare_and_swap {
            missing.push("finish_source_compare_and_swap");
        }
        missing
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
