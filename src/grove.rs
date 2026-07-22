//! The only place summoner talks to grove: subprocess CLI + JSON, never the
//! crate. Mirrors are lenient (unknown fields ignored) so grove can grow its
//! schema; the protocol we require is gated once by [`GroveCli::preflight`].
//!
//! Classification rule, matching grove's contract: parseable JSON on stdout is
//! a domain outcome (exit 0 or 1); anything else is an error with grove's
//! stderr attached.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "grove_compat.rs"]
mod compat;
#[path = "grove_inspection.rs"]
mod inspection;
pub use compat::Capabilities;
pub use inspection::InspectionExec;

pub struct GroveCli {
    bin: String,
}

struct GroveOutput {
    code: i32,
    stdout: String,
    stderr: String,
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum BeginOutcome {
    Begun { task: TaskInfo },
    Conflict { conflicts: Vec<serde_json::Value> },
}

#[derive(Deserialize)]
pub struct TaskInfo {
    pub id: String,
}

#[derive(Deserialize, serde::Serialize, Default, Clone)]
#[serde(default)]
pub struct TaskVerification {
    pub required: Vec<String>,
    pub passed: Vec<String>,
    pub missing: Vec<String>,
    pub stale: Vec<String>,
    pub failed: Vec<String>,
    pub verified: bool,
}

pub enum FinishOutcome {
    Finished {
        verification: TaskVerification,
    },
    Refused {
        reason: String,
        outside_scope: Vec<String>,
        verification: Option<TaskVerification>,
    },
}

#[derive(Deserialize, serde::Serialize, Default, Clone)]
#[serde(default)]
pub struct VerifySummary {
    pub profile: String,
    pub run_id: String,
    pub passed: bool,
    pub receipts: Vec<ReceiptSummary>,
}

#[derive(Deserialize, serde::Serialize, Default, Clone)]
#[serde(default)]
pub struct ReceiptSummary {
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub passed: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Deserialize, serde::Serialize, Default)]
#[serde(default)]
pub struct ReleaseOutcome {
    pub branch: Option<String>,
    pub saved_to: Option<String>,
}

impl GroveCli {
    pub fn new(bin: String) -> Self {
        Self { bin }
    }

    fn call(&self, cwd: &Path, args: &[&str]) -> Result<GroveOutput> {
        let output = Command::new(&self.bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("running {} {}", self.bin, args.join(" ")))?;
        Ok(GroveOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// A domain call: JSON on stdout with exit 0 or 1. Any other exit is an
    /// error even if stdout parses — a grove that printed JSON and then died
    /// (or exited 2) did not deliver a domain outcome.
    pub(super) fn domain(&self, cwd: &Path, args: &[&str]) -> Result<serde_json::Value> {
        let out = self.call(cwd, args)?;
        if matches!(out.code, 0 | 1)
            && let Ok(value) = serde_json::from_str(out.stdout.trim())
        {
            return Ok(value);
        }
        bail!(
            "grove {} failed (exit {}): {}",
            args.first().copied().unwrap_or_default(),
            out.code,
            out.stderr.trim()
        )
    }

    pub fn version(&self) -> Result<String> {
        let out = self.call(Path::new("."), &["--version"])?;
        if out.code != 0 {
            bail!("{} --version failed: {}", self.bin, out.stderr.trim());
        }
        Ok(out.stdout.trim().to_string())
    }

    pub fn preflight(&self) -> Result<Capabilities> {
        compat::check(self)
    }

    pub fn cargo_generate_lockfile(&self, repo: &Path) -> Result<()> {
        let out = self.call(
            repo,
            &[
                "exec",
                "--tag",
                "summoner-init-lock",
                "--",
                "cargo",
                "generate-lockfile",
            ],
        )?;
        if out.code != 0 {
            bail!(
                "grove could not generate Cargo.lock for the demo (exit {}): {}",
                out.code,
                out.stderr.trim()
            );
        }
        if !repo.join("Cargo.lock").is_file() {
            bail!("grove completed lockfile generation without creating Cargo.lock");
        }
        Ok(())
    }

    /// `worktree acquire` prints one bare path on stdout, not JSON.
    pub fn worktree_acquire(
        &self,
        repo: &Path,
        agent: &str,
        branch: Option<&str>,
        base: Option<&str>,
    ) -> Result<PathBuf> {
        let mut args = vec!["worktree", "acquire", "--agent", agent];
        if let Some(branch) = branch {
            args.extend(["--branch", branch]);
        }
        if let Some(base) = base {
            args.extend(["--base", base]);
        }
        let out = self.call(repo, &args)?;
        if out.code != 0 {
            bail!("grove worktree acquire failed: {}", out.stderr.trim());
        }
        let path = PathBuf::from(out.stdout.trim());
        if !path.is_dir() {
            bail!(
                "grove worktree acquire printed a path that does not exist: {}",
                path.display()
            );
        }
        Ok(path)
    }

    pub fn task_begin(
        &self,
        worktree: &Path,
        agent: &str,
        title: &str,
        scope: &[String],
        claim_group: Option<&str>,
    ) -> Result<BeginOutcome> {
        let mut args = vec![
            "task", "begin", "--agent", agent, "--task", title, "--scope",
        ];
        args.extend(scope.iter().map(String::as_str));
        if let Some(group) = claim_group {
            args.extend(["--claim-group", group]);
        }
        let value = self.domain(worktree, &args)?;
        parse_begin(value)
    }

    /// The full argv for one supervised executor run; executor.rs spawns it.
    /// grove owns the deadline, so the executor dies on time even if summoner
    /// is killed first.
    pub fn exec_argv(&self, task_id: &str, timeout_secs: u64, executor: &[String]) -> Vec<String> {
        // Executors are agent sessions, not build commands: the edit capability
        // supervises lifetime and deadline without holding a build lane or
        // admission slot, so the builds an agent runs acquire lanes on demand
        // and a fleet is never throttled to max_builders live sessions.
        let mut argv = vec![
            self.bin.clone(),
            "task".into(),
            "exec".into(),
            "--capability".into(),
            "edit".into(),
            "--task-id".into(),
            task_id.into(),
            "--timeout-secs".into(),
            timeout_secs.to_string(),
            "--".into(),
        ];
        argv.extend(executor.iter().cloned());
        argv
    }

    pub fn verify(&self, worktree: &Path, profile: &str, task_id: &str) -> Result<VerifySummary> {
        let value = self.domain(worktree, &["verify", profile, "--task-id", task_id])?;
        serde_json::from_value(value).context("parsing grove verify report")
    }

    pub fn task_finish(
        &self,
        worktree: &Path,
        task_id: &str,
        allow_unverified: Option<&str>,
        expected_source_sha256: Option<&str>,
    ) -> Result<FinishOutcome> {
        let mut args = vec!["task", "finish", "--task-id", task_id];
        if let Some(reason) = allow_unverified {
            args.extend(["--allow-unverified", reason]);
        }
        if let Some(digest) = expected_source_sha256 {
            args.extend(["--expected-source-sha256", digest]);
        }
        let value = self.domain(worktree, &args)?;
        parse_finish(value, expected_source_sha256)
    }

    pub fn task_abandon(&self, worktree: &Path, task_id: &str, reason: &str) -> Result<()> {
        self.domain(
            worktree,
            &["task", "abandon", "--task-id", task_id, "--reason", reason],
        )
        .map(|_| ())
    }

    /// Fails when grove refuses the release (live lane, switched branch); the
    /// caller records that as needing manual recovery — reap will not fix a
    /// checkout that left its leased branch.
    pub fn worktree_release(&self, repo: &Path, worktree: &Path) -> Result<ReleaseOutcome> {
        let path = worktree.display().to_string();
        let value = self.domain(repo, &["worktree", "release", &path])?;
        serde_json::from_value(value).context("parsing grove release outcome")
    }

    pub fn task_status(&self, repo: &Path) -> Result<serde_json::Value> {
        self.domain(repo, &["task", "status", "--json"])
    }

    /// Partition analysis: proposed scope sets in, conflicts / couplings /
    /// waves out. Exit 1 means conflicts were found — still a verdict.
    pub fn partition(&self, repo: &Path, sets: &serde_json::Value) -> Result<serde_json::Value> {
        use std::io::Write;
        let mut child = Command::new(&self.bin)
            .args(["plan", "--partition"])
            .current_dir(repo)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("running {} plan --partition", self.bin))?;
        child
            .stdin
            .take()
            .context("partition stdin unavailable")?
            .write_all(sets.to_string().as_bytes())
            .context("writing scope sets")?;
        let output = child
            .wait_with_output()
            .context("waiting for grove plan --partition")?;
        let code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        if matches!(code, 0 | 1)
            && let Ok(value) = serde_json::from_str(stdout.trim())
        {
            return Ok(value);
        }
        bail!(
            "grove plan --partition failed (exit {code}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn parse_begin(value: serde_json::Value) -> Result<BeginOutcome> {
    serde_json::from_value(value).context("parsing grove task begin outcome")
}

/// Success has no "outcome" key (stable FinishReport shape); refusals do.
fn parse_finish(
    value: serde_json::Value,
    expected_source_sha256: Option<&str>,
) -> Result<FinishOutcome> {
    if value.get("outcome").is_some() {
        Ok(FinishOutcome::Refused {
            reason: value["reason"].as_str().unwrap_or("unknown").to_string(),
            outside_scope: serde_json::from_value(value["outside_scope"].clone())
                .unwrap_or_default(),
            verification: serde_json::from_value(value["verification"].clone()).ok(),
        })
    } else {
        let source_sha256 = value["source_sha256"].as_str().map(String::from);
        if let Some(expected) = expected_source_sha256
            && source_sha256.as_deref() != Some(expected)
        {
            bail!("Grove finish did not return the expected candidate source digest")
        }
        Ok(FinishOutcome::Finished {
            verification: serde_json::from_value(value["verification"].clone())
                .context("parsing finish verification")?,
        })
    }
}

#[cfg(test)]
#[path = "grove_tests.rs"]
mod tests;
