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
        let mut argv = vec![
            self.bin.clone(),
            "task".into(),
            "exec".into(),
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
    ) -> Result<FinishOutcome> {
        let mut args = vec!["task", "finish", "--task-id", task_id];
        if let Some(reason) = allow_unverified {
            args.extend(["--allow-unverified", reason]);
        }
        let value = self.domain(worktree, &args)?;
        parse_finish(value)
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
fn parse_finish(value: serde_json::Value) -> Result<FinishOutcome> {
    if value.get("outcome").is_some() {
        Ok(FinishOutcome::Refused {
            reason: value["reason"].as_str().unwrap_or("unknown").to_string(),
            outside_scope: serde_json::from_value(value["outside_scope"].clone())
                .unwrap_or_default(),
            verification: serde_json::from_value(value["verification"].clone()).ok(),
        })
    } else {
        Ok(FinishOutcome::Finished {
            verification: serde_json::from_value(value["verification"].clone())
                .context("parsing finish verification")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_outcomes_parse_both_arms() {
        let begun: BeginOutcome = serde_json::from_str(
            r#"{"outcome":"begun","task":{"id":"abc-1","agent":"smn-x","extra_future_field":1}}"#,
        )
        .unwrap();
        let BeginOutcome::Begun { task } = begun else {
            panic!("expected begun");
        };
        assert_eq!(task.id, "abc-1");

        let conflict: BeginOutcome = serde_json::from_str(
            r#"{"outcome":"conflict","requested":["src"],"conflicts":[{"agent":"other"}]}"#,
        )
        .unwrap();
        let BeginOutcome::Conflict { conflicts } = conflict else {
            panic!("expected conflict");
        };
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn finish_success_and_refusals_are_distinguished_by_the_outcome_key() {
        let finished = parse_finish(serde_json::json!({
            "task": {"id": "t", "verification": "passed"},
            "verification": {"required": ["fast"], "passed": ["fast"], "missing": [],
                             "stale": [], "failed": [], "verified": true}
        }))
        .unwrap();
        let FinishOutcome::Finished { verification } = finished else {
            panic!("expected finished");
        };
        assert!(verification.verified);

        let refused = parse_finish(serde_json::json!({
            "outcome": "refused", "reason": "evidence",
            "verification": {"required": ["fast", "ci"], "passed": [], "missing": ["fast", "ci"],
                             "stale": [], "failed": [], "verified": false}
        }))
        .unwrap();
        let FinishOutcome::Refused {
            reason,
            verification,
            ..
        } = refused
        else {
            panic!("expected refusal");
        };
        assert_eq!(reason, "evidence");
        assert_eq!(verification.unwrap().missing, ["fast", "ci"]);

        let scope = parse_finish(serde_json::json!({
            "outcome": "refused", "reason": "scope", "outside_scope": ["README.md"]
        }))
        .unwrap();
        let FinishOutcome::Refused { outside_scope, .. } = scope else {
            panic!("expected refusal");
        };
        assert_eq!(outside_scope, ["README.md"]);
    }

    #[test]
    fn exec_argv_wraps_the_executor_behind_the_supervisor() {
        let grove = GroveCli::new("grove".into());
        let argv = grove.exec_argv("t-1", 900, &["codex".into(), "exec".into()]);
        assert_eq!(
            argv,
            [
                "grove",
                "task",
                "exec",
                "--task-id",
                "t-1",
                "--timeout-secs",
                "900",
                "--",
                "codex",
                "exec"
            ]
        );
    }
}
