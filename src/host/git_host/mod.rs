//! Default host: git worktrees, claim registry, task ledger, Summoner supervision.

mod util;
use util::*;

use super::Host;
use super::git_claim::ClaimStore;
use super::git_ledger::{self, Ledger, TaskRecord, TaskState};
use super::partition;
use super::types::{ExecutionPlan, HostCapabilities, HostInfo, TaskBeginRequest, WorktreeRequest};
use super::verify_config::VerificationConfig;
use crate::grove::{
    BeginOutcome, FinishOutcome, InspectionAcquire, InspectionExec, InspectionLog, ReleaseOutcome,
    TaskInfo, TaskVerification, VerifySummary,
};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitHost {
    repo: PathBuf,
    worktree_root: PathBuf,
    state_root: PathBuf,
    claims: ClaimStore,
    ledger: Ledger,
    verify: VerificationConfig,
    repo_slug: String,
}

impl GitHost {
    pub fn new(
        repo: &Path,
        worktree_root: Option<PathBuf>,
        verify: VerificationConfig,
    ) -> Result<Self> {
        if cfg!(windows) {
            bail!(
                "git host is not yet supported on Windows; set [host] kind = \"grove\" or run on Unix"
            );
        }
        let repo = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
        let slug = repo_slug(&repo);
        let cache = runs_parent().join("host-git").join(&slug);
        std::fs::create_dir_all(&cache)?;
        let worktree_root = worktree_root.unwrap_or_else(|| cache.join("worktrees"));
        std::fs::create_dir_all(&worktree_root)?;
        let state_root = cache.join("state");
        std::fs::create_dir_all(&state_root)?;
        Ok(Self {
            claims: ClaimStore::open(&state_root, &slug, 1800)?,
            ledger: Ledger::open(&state_root)?,
            repo,
            worktree_root,
            state_root,
            verify,
            repo_slug: slug,
        })
    }
}

impl Host for GitHost {
    fn kind(&self) -> &str {
        "git"
    }

    fn preflight(&self) -> Result<HostInfo> {
        let out = Command::new("git")
            .arg("--version")
            .output()
            .context("git not available for git host")?;
        if !out.status.success() {
            bail!("git --version failed");
        }
        let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(HostInfo {
            kind: "git".into(),
            version,
            state_root: Some(self.state_root.clone()),
            capabilities: self.capabilities(),
        })
    }

    fn capabilities(&self) -> HostCapabilities {
        let mut protected = vec![".summoner.toml".into()];
        if self.verify.profiles.values().any(|p| {
            p.commands.iter().any(|c| {
                c.argv
                    .first()
                    .is_some_and(|a| a.contains("cargo") || a == "rustc")
            })
        }) {
            protected.extend([
                "rust-toolchain".into(),
                "rust-toolchain.toml".into(),
                ".cargo/config".into(),
                ".cargo/config.toml".into(),
            ]);
        }
        // Exact-state holds only for a *clean committed* candidate: verify,
        // review capsule, and finish all refuse a dirty worktree, then bind
        // HEAD (see require_clean_candidate). This is not Grove's full
        // workspace snapshot, but it closes the dirty-after-verify hole.
        HostCapabilities {
            supervised_exec: true,
            receipt_finish: true,
            cargo_topology: false,
            cow_lanes: false,
            inspection_capsule: true,
            scope_includes_committed_delta: true,
            verification_bound_to_source: true,
            immutable_inspection_snapshot: true,
            review_process_isolated: true,
            finish_source_compare_and_swap: true,
            protected_paths: protected,
        }
    }

    fn worktree_acquire(&self, req: WorktreeRequest<'_>) -> Result<PathBuf> {
        let branch = req
            .branch
            .map(String::from)
            .unwrap_or_else(|| format!("smn/{}-{}", req.order_id, req.attempt));
        let path = self.worktree_root.join(format!(
            "{}-{}-{}",
            sanitize(req.run_id),
            sanitize(req.order_id),
            req.attempt
        ));
        if path.exists() {
            bail!("worktree path already exists: {}", path.display());
        }
        let base = req.base.unwrap_or("HEAD");
        // Create branch from base if needed, then add worktree.
        let _ = Command::new("git")
            .args(["rev-parse", "--verify", &branch])
            .current_dir(&self.repo)
            .output();
        let exists = Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !exists {
            let st = Command::new("git")
                .args(["branch", &branch, base])
                .current_dir(&self.repo)
                .status()
                .context("git branch")?;
            if !st.success() {
                // base might be a commit; try checkout -b via worktree add -b
                let st = Command::new("git")
                    .args([
                        "worktree",
                        "add",
                        "-b",
                        &branch,
                        &path.to_string_lossy(),
                        base,
                    ])
                    .current_dir(&self.repo)
                    .status()
                    .context("git worktree add -b")?;
                if !st.success() {
                    bail!("git worktree add -b failed for {branch}");
                }
                return Ok(path);
            }
        }
        let st = Command::new("git")
            .args(["worktree", "add", &path.to_string_lossy(), &branch])
            .current_dir(&self.repo)
            .status()
            .context("git worktree add")?;
        if !st.success() {
            bail!("git worktree add failed for {}", path.display());
        }
        Ok(path)
    }

    fn worktree_release(&self, _repo: &Path, worktree: &Path) -> Result<ReleaseOutcome> {
        // Salvage dirty work onto the branch before remove.
        let _ = Command::new("git")
            .args(["add", "-A"])
            .current_dir(worktree)
            .status();
        let dirty = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree)
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false);
        let mut saved_to = None;
        let branch = git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
        if dirty {
            let _ = Command::new("git")
                .args([
                    "commit",
                    "--allow-empty",
                    "-m",
                    "summoner: salvage dirty worktree",
                ])
                .current_dir(worktree)
                .status();
            saved_to = branch.clone();
        }
        let path = worktree.display().to_string();
        let st = Command::new("git")
            .args(["worktree", "remove", "--force", &path])
            .current_dir(&self.repo)
            .status()
            .context("git worktree remove")?;
        if !st.success() {
            bail!("git worktree remove failed for {path}");
        }
        Ok(ReleaseOutcome { branch, saved_to })
    }

    fn task_begin(&self, req: TaskBeginRequest<'_>) -> Result<BeginOutcome> {
        let id = git_ledger::new_task_id();
        if let Err(conflicts) = self
            .claims
            .begin(&id, req.agent, req.scope, req.claim_group)?
        {
            return Ok(BeginOutcome::Conflict { conflicts });
        }
        let branch = git_out(
            req.worktree,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .ok();
        let start_commit =
            git_out(req.worktree, &["rev-parse", "HEAD"]).context("recording task start commit")?;
        let rec = TaskRecord {
            schema_version: 2,
            id: id.clone(),
            run_id: req.run_id.into(),
            order_id: req.order_id.into(),
            attempt: req.attempt,
            agent: req.agent.into(),
            title: req.title.into(),
            scope: req.scope.to_vec(),
            claim_group: req.claim_group.map(String::from),
            branch,
            worktree: req.worktree.display().to_string(),
            start_commit,
            verify_source_commit: None,
            verify_source_sha256: None,
            state: TaskState::Begun,
            verification: TaskVerification::default(),
            owner_pid: std::process::id(),
            updated_at: git_ledger::now_secs(),
        };
        self.ledger.write(&rec)?;
        Ok(BeginOutcome::Begun {
            task: TaskInfo { id },
        })
    }

    fn execution_plan(
        &self,
        task_id: &str,
        timeout_secs: u64,
        executor: &[String],
    ) -> Result<ExecutionPlan> {
        let _ = self.ledger.set_state(task_id, TaskState::Executing);
        let _ = self.claims.renew(task_id);
        Ok(ExecutionPlan::SummonerSupervised {
            argv: executor.to_vec(),
            timeout_secs,
        })
    }

    fn task_finish(
        &self,
        worktree: &Path,
        task_id: &str,
        allow_unverified: Option<&str>,
        expected_source_sha256: Option<&str>,
    ) -> Result<FinishOutcome> {
        let rec = self.ledger.read(task_id)?;
        git_ledger::require_active(&rec)?;
        let required = if self.verify.required.is_empty() {
            self.verify.profiles.keys().cloned().collect::<Vec<_>>()
        } else {
            self.verify.required.clone()
        };

        // Dirty tree means verify could not have bound the bytes under HEAD.
        if let Err(err) = require_clean_candidate(worktree) {
            let _ = self.claims.release(task_id);
            let _ = self.ledger.set_state(task_id, TaskState::Refused);
            return Ok(FinishOutcome::Refused {
                reason: "source_changed".into(),
                outside_scope: vec![err.to_string()],
                verification: None,
            });
        }

        let outside = outside_scope_paths(worktree, &rec)?;
        if !outside.is_empty() {
            let _ = self.claims.release(task_id);
            let _ = self.ledger.set_state(task_id, TaskState::Refused);
            return Ok(FinishOutcome::Refused {
                reason: "scope".into(),
                outside_scope: outside,
                verification: None,
            });
        }

        let head = git_out(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
        let head_digest = candidate_source_digest(worktree)?;
        if let Some(expected) = expected_source_sha256
            && expected != head_digest
        {
            let _ = self.claims.release(task_id);
            let _ = self.ledger.set_state(task_id, TaskState::Refused);
            return Ok(FinishOutcome::Refused {
                reason: "source_changed".into(),
                outside_scope: vec![],
                verification: None,
            });
        }

        let mut verification = TaskVerification {
            required: required.clone(),
            passed: rec.verification.passed.clone(),
            missing: Vec::new(),
            stale: Vec::new(),
            failed: rec.verification.failed.clone(),
            verified: false,
        };
        // Profiles bound to a prior candidate become stale if HEAD moved.
        if let Some(bound) = rec.verify_source_commit.as_deref()
            && bound != head.as_str()
        {
            for profile in verification.passed.drain(..) {
                if !verification.stale.contains(&profile) {
                    verification.stale.push(profile);
                }
            }
        }
        for profile in &required {
            if !verification.passed.contains(profile) {
                if verification.failed.contains(profile) || verification.stale.contains(profile) {
                    continue;
                }
                verification.missing.push(profile.clone());
            }
        }
        verification.verified = !required.is_empty()
            && verification.missing.is_empty()
            && verification.failed.is_empty()
            && verification.stale.is_empty();

        if required.is_empty() {
            let _ = self.claims.release(task_id);
            self.ledger
                .set_verification(task_id, verification.clone(), TaskState::Finished)?;
            return Ok(FinishOutcome::Finished { verification });
        }

        if !verification.verified {
            if allow_unverified.is_some() {
                let _ = self.claims.release(task_id);
                self.ledger
                    .set_verification(task_id, verification.clone(), TaskState::Finished)?;
                return Ok(FinishOutcome::Finished { verification });
            }
            let _ = self.claims.release(task_id);
            self.ledger
                .set_verification(task_id, verification.clone(), TaskState::Refused)?;
            return Ok(FinishOutcome::Refused {
                reason: "evidence".into(),
                outside_scope: vec![],
                verification: Some(verification),
            });
        }
        let _ = self.claims.release(task_id);
        self.ledger
            .set_verification(task_id, verification.clone(), TaskState::Finished)?;
        Ok(FinishOutcome::Finished { verification })
    }

    fn task_abandon(&self, _worktree: &Path, task_id: &str, _reason: &str) -> Result<()> {
        let _ = self.claims.release(task_id);
        let _ = self.ledger.set_state(task_id, TaskState::Abandoned);
        Ok(())
    }

    fn task_status(&self, _repo: &Path) -> Result<serde_json::Value> {
        // Shape matches Grove task status schema 4 fields Summoner resume reads.
        let tasks: Vec<_> = self
            .ledger
            .list()?
            .into_iter()
            .map(|t| {
                let status = match t.state {
                    TaskState::Finished | TaskState::Refused => "finished",
                    TaskState::Abandoned => "abandoned",
                    TaskState::Begun | TaskState::Executing | TaskState::Verifying => "running",
                };
                let recorded_verification = if t.verification.verified {
                    "passed"
                } else {
                    "unverified"
                };
                serde_json::json!({
                    "id": t.id,
                    "status": status,
                    "recorded_verification": recorded_verification,
                    "source_sha256": t.verify_source_sha256,
                    "start_commit": t.start_commit,
                    "branch": t.branch,
                    "worktree": t.worktree,
                    "agent": t.agent,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "schema_version": 4,
            "host": "git",
            "repo_slug": self.repo_slug,
            "tasks": tasks,
        }))
    }

    fn kill_supervised(&self, _task_id: &str, _worktree: &Path) -> Result<()> {
        Ok(())
    }

    fn verify(&self, worktree: &Path, profile: &str, task_id: &str) -> Result<VerifySummary> {
        let _ = self.ledger.set_state(task_id, TaskState::Verifying);
        let _ = self.claims.renew(task_id);
        // Exact-state contract: only a clean committed tree may verify.
        require_clean_candidate(worktree).with_context(|| {
            format!("git host refuses verify on a dirty worktree (task {task_id})")
        })?;
        let Some(prof) = self.verify.profiles.get(profile) else {
            // Missing profile is a hard miss, not a green checkmark.
            let mut rec = self.ledger.read(task_id)?;
            if !rec.verification.failed.contains(&profile.to_string()) {
                rec.verification.failed.push(profile.into());
            }
            rec.verification.passed.retain(|p| p != profile);
            rec.verification.verified = false;
            self.ledger.write(&rec)?;
            return Ok(VerifySummary {
                profile: profile.into(),
                run_id: format!("git-{task_id}"),
                passed: false,
                receipts: vec![],
            });
        };
        if prof.commands.is_empty() || prof.commands.iter().all(|c| c.argv.is_empty()) {
            let mut rec = self.ledger.read(task_id)?;
            if !rec.verification.failed.contains(&profile.to_string()) {
                rec.verification.failed.push(profile.into());
            }
            rec.verification.verified = false;
            self.ledger.write(&rec)?;
            return Ok(VerifySummary {
                profile: profile.into(),
                run_id: format!("git-{task_id}"),
                passed: false,
                receipts: vec![],
            });
        }
        let mut receipts = Vec::new();
        let mut passed = true;
        for cmd in &prof.commands {
            if cmd.argv.is_empty() {
                continue;
            }
            let output = Command::new(&cmd.argv[0])
                .args(&cmd.argv[1..])
                .current_dir(worktree)
                .output()
                .with_context(|| format!("running verify {}", cmd.argv[0]))?;
            let code = output.status.code().unwrap_or(-1);
            let ok = code == 0;
            if !ok {
                passed = false;
            }
            receipts.push(crate::grove::ReceiptSummary {
                argv: cmd.argv.clone(),
                exit_code: Some(code),
                passed: ok,
                duration_ms: None,
            });
        }
        // Re-check cleanliness: a verify command must not leave dirty state and
        // still mint a green profile bound only to HEAD.
        if passed {
            require_clean_candidate(worktree).with_context(|| {
                format!("verify profile {profile:?} left the worktree dirty; cannot bind HEAD")
            })?;
        }
        let mut rec = self.ledger.read(task_id)?;
        if passed {
            if !rec.verification.passed.contains(&profile.to_string()) {
                rec.verification.passed.push(profile.into());
            }
            rec.verification.failed.retain(|p| p != profile);
            let head = git_out(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
            rec.verify_source_commit = Some(head);
            rec.verify_source_sha256 = Some(candidate_source_digest(worktree)?);
        } else if !rec.verification.failed.contains(&profile.to_string()) {
            rec.verification.failed.push(profile.into());
            rec.verification.passed.retain(|p| p != profile);
            rec.verify_source_commit = None;
            rec.verify_source_sha256 = None;
        }
        self.ledger.write(&rec)?;
        Ok(VerifySummary {
            profile: profile.into(),
            run_id: format!("git-{task_id}"),
            passed,
            receipts,
        })
    }

    fn partition(&self, _repo: &Path, sets: &serde_json::Value) -> Result<serde_json::Value> {
        Ok(partition::partition(sets))
    }

    fn inspection_acquire(
        &self,
        worktree: &Path,
        task_id: &str,
        _lease_secs: u64,
    ) -> Result<InspectionAcquire> {
        // Private capsule: detached worktree pinned at the clean candidate commit.
        require_clean_candidate(worktree)
            .context("git host refuses review capsule on a dirty worktree")?;
        let head = git_out(worktree, &["rev-parse", "HEAD"]).context("capsule head")?;
        let source_sha256 = candidate_source_digest(worktree)?;
        let capsule_id = format!("git-capsule-{task_id}");
        let capsule_path = self.state_root.join("capsules").join(&capsule_id);
        if capsule_path.exists() {
            let _ = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &capsule_path.to_string_lossy(),
                ])
                .current_dir(worktree)
                .status();
            let _ = std::fs::remove_dir_all(&capsule_path);
        }
        std::fs::create_dir_all(capsule_path.parent().unwrap())?;
        let st = Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                &capsule_path.to_string_lossy(),
                &head,
            ])
            .current_dir(worktree)
            .status()
            .context("creating private review worktree")?;
        if !st.success() {
            bail!("git worktree add --detach failed for inspection capsule {capsule_id}");
        }
        let meta = serde_json::json!({
            "task_id": task_id,
            "source_sha256": source_sha256,
            "head": head,
            "live_worktree": worktree,
            "capsule_path": capsule_path,
        });
        std::fs::write(
            self.state_root.join(format!("{capsule_id}.json")),
            serde_json::to_vec_pretty(&meta)?,
        )?;
        Ok(InspectionAcquire {
            schema_version: 1,
            capsule_id,
            path: capsule_path,
            task_id: task_id.into(),
            source_sha256,
        })
    }

    fn inspection_exec(
        &self,
        worktree: &Path,
        capsule_id: &str,
        timeout_secs: u64,
        argv: &[String],
    ) -> Result<InspectionExec> {
        use super::supervise;
        use std::sync::atomic::AtomicBool;
        let meta_path = self.state_root.join(format!("{capsule_id}.json"));
        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&meta_path).context("reading inspection capsule meta")?,
        )?;
        let task_id = meta["task_id"].as_str().unwrap_or("").to_string();
        let bound = meta["source_sha256"].as_str().unwrap_or("").to_string();
        let bound_head = meta["head"].as_str().unwrap_or("").to_string();
        let capsule_path = PathBuf::from(meta["capsule_path"].as_str().unwrap_or(""));
        if !capsule_path.is_dir() {
            bail!(
                "inspection capsule path missing: {}",
                capsule_path.display()
            );
        }
        let stdout_path = self.state_root.join(format!("{capsule_id}-stdout.log"));
        let stderr_path = self.state_root.join(format!("{capsule_id}-stderr.log"));
        let stdout = std::fs::File::create(&stdout_path)?;
        let stderr = std::fs::File::create(&stderr_path)?;
        let shutdown = AtomicBool::new(false);
        let outcome = supervise::run(
            argv,
            &capsule_path,
            timeout_secs,
            None,
            std::process::Stdio::from(stdout),
            std::process::Stdio::from(stderr),
            &shutdown,
        )?;
        let capsule_head = git_out(&capsule_path, &["rev-parse", "HEAD"]).unwrap_or_default();
        let live_head = git_out(worktree, &["rev-parse", "HEAD"]).unwrap_or_default();
        let live_clean = require_clean_candidate(worktree).is_ok();
        let live_digest = candidate_source_digest(worktree).unwrap_or_default();
        // Live candidate must still be the bound clean HEAD; capsule must match.
        let source_unchanged = live_clean && live_digest == bound && live_head == bound_head;
        let capsule_unchanged = capsule_head == bound_head && capsule_path.is_dir();
        let tree_clean = git_out(&capsule_path, &["status", "--porcelain"])
            .map(|s| s.trim().is_empty())
            .unwrap_or(false);
        let stdout_bytes = std::fs::read(&stdout_path).unwrap_or_default();
        let stderr_bytes = std::fs::read(&stderr_path).unwrap_or_default();
        let authorized =
            outcome.exit == Some(0) && tree_clean && source_unchanged && capsule_unchanged;
        Ok(InspectionExec {
            schema_version: 1,
            capsule_id: capsule_id.into(),
            task_id,
            exit_code: outcome.exit.unwrap_or(-1),
            timed_out: outcome.backup_killed,
            tree_clean,
            source_unchanged,
            capsule_unchanged,
            authorized,
            source_sha256: bound,
            stdout: InspectionLog {
                path: stdout_path,
                sha256: sha256_hex(&stdout_bytes),
                bytes: stdout_bytes.len() as u64,
            },
            stderr: InspectionLog {
                path: stderr_path,
                sha256: sha256_hex(&stderr_bytes),
                bytes: stderr_bytes.len() as u64,
            },
        })
    }

    fn inspection_release(&self, worktree: &Path, capsule_id: &str) -> Result<()> {
        let meta_path = self.state_root.join(format!("{capsule_id}.json"));
        if let Ok(bytes) = std::fs::read(&meta_path)
            && let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&bytes)
        {
            let capsule_path = PathBuf::from(meta["capsule_path"].as_str().unwrap_or(""));
            if !capsule_path.as_os_str().is_empty() {
                let _ = Command::new("git")
                    .args([
                        "worktree",
                        "remove",
                        "--force",
                        &capsule_path.to_string_lossy(),
                    ])
                    .current_dir(worktree)
                    .status();
                let _ = std::fs::remove_dir_all(&capsule_path);
            }
        }
        let _ = std::fs::remove_file(meta_path);
        let _ = std::fs::remove_file(self.state_root.join(format!("{capsule_id}-stdout.log")));
        let _ = std::fs::remove_file(self.state_root.join(format!("{capsule_id}-stderr.log")));
        Ok(())
    }
}
