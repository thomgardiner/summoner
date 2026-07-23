//! File-backed claim registry for the git host.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub agent: String,
    pub scope: Vec<String>,
    pub claim_group: Option<String>,
    pub task_id: String,
    pub expires_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    claims: BTreeMap<String, Claim>,
}

pub struct ClaimStore {
    path: PathBuf,
    ttl_secs: u64,
}

impl ClaimStore {
    pub fn open(state_root: &Path, repo_slug: &str, ttl_secs: u64) -> Result<Self> {
        let dir = state_root.join("claims");
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            path: dir.join(format!("{repo_slug}.json")),
            ttl_secs,
        })
    }

    pub fn begin(
        &self,
        task_id: &str,
        agent: &str,
        scope: &[String],
        claim_group: Option<&str>,
    ) -> Result<Result<(), Vec<serde_json::Value>>> {
        let mut reg = self.load()?;
        let now = now_secs();
        reg.claims.retain(|_, c| c.expires_at > now);

        let conflicts: Vec<_> = reg
            .claims
            .values()
            .filter(|c| scopes_overlap(&c.scope, scope))
            .filter(|c| !matches!((&c.claim_group, claim_group), (Some(a), Some(b)) if a == b))
            .map(|c| {
                serde_json::json!({
                    "agent": c.agent,
                    "task_id": c.task_id,
                    "scope": c.scope,
                })
            })
            .collect();
        if !conflicts.is_empty() {
            return Ok(Err(conflicts));
        }
        reg.claims.insert(
            task_id.to_string(),
            Claim {
                agent: agent.into(),
                scope: scope.to_vec(),
                claim_group: claim_group.map(String::from),
                task_id: task_id.into(),
                expires_at: now.saturating_add(self.ttl_secs),
            },
        );
        self.store(&reg)?;
        Ok(Ok(()))
    }

    pub fn release(&self, task_id: &str) -> Result<()> {
        let mut reg = self.load()?;
        reg.claims.remove(task_id);
        self.store(&reg)
    }

    pub fn renew(&self, task_id: &str) -> Result<()> {
        let mut reg = self.load()?;
        if let Some(c) = reg.claims.get_mut(task_id) {
            c.expires_at = now_secs().saturating_add(self.ttl_secs);
            self.store(&reg)?;
        }
        Ok(())
    }

    fn load(&self) -> Result<Registry> {
        if !self.path.exists() {
            return Ok(Registry::default());
        }
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("reading claims {}", self.path.display()))?;
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn store(&self, reg: &Registry) -> Result<()> {
        let parent = self.path.parent().context("claims path parent")?;
        std::fs::create_dir_all(parent)?;
        let temp = parent.join(format!(".claims-{}-{}.tmp", std::process::id(), now_secs()));
        let bytes = serde_json::to_vec_pretty(reg)?;
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }
}

fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    for x in a {
        for y in b {
            if x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/")) {
                return true;
            }
        }
    }
    false
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn conflict_and_group_race() {
        let dir = tempdir().unwrap();
        let store = ClaimStore::open(dir.path(), "repo", 3600).unwrap();
        assert!(
            store
                .begin("t1", "a", &["src".into()], None)
                .unwrap()
                .is_ok()
        );
        assert!(
            store
                .begin("t2", "b", &["src/lib.rs".into()], None)
                .unwrap()
                .is_err()
        );
        assert!(
            store
                .begin("t3", "c", &["src".into()], Some("race"))
                .unwrap()
                .is_err()
        );
        store.release("t1").unwrap();
        assert!(
            store
                .begin("t4", "d", &["src".into()], Some("race"))
                .unwrap()
                .is_ok()
        );
        assert!(
            store
                .begin("t5", "e", &["src".into()], Some("race"))
                .unwrap()
                .is_ok()
        );
    }
}
