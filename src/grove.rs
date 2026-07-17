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

/// grove release with `task exec --timeout-secs` and structured finish refusals.
const REQUIRED_VERSION: (u64, u64, u64) = (0, 3, 2);

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
    #[serde(deserialize_with = "receipt_summaries", rename = "receipts")]
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

fn receipt_summaries<'de, D>(deserializer: D) -> Result<Vec<ReceiptSummary>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<ReceiptSummary>::deserialize(deserializer)
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

    /// A domain call: JSON on stdout regardless of exit 0/1; no JSON is an error.
    fn domain(&self, cwd: &Path, args: &[&str]) -> Result<serde_json::Value> {
        let out = self.call(cwd, args)?;
        match serde_json::from_str(out.stdout.trim()) {
            Ok(value) => Ok(value),
            Err(_) => bail!(
                "grove {} failed (exit {}): {}",
                args.first().copied().unwrap_or_default(),
                out.code,
                out.stderr.trim()
            ),
        }
    }

    pub fn version(&self) -> Result<String> {
        let out = self.call(Path::new("."), &["--version"])?;
        if out.code != 0 {
            bail!("{} --version failed: {}", self.bin, out.stderr.trim());
        }
        Ok(out.stdout.trim().to_string())
    }

    pub fn preflight(&self) -> Result<()> {
        let version = self
            .version()
            .with_context(|| format!("grove binary {:?} not usable", self.bin))?;
        let numbers = version
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .chain(std::iter::repeat(0))
            .take(3)
            .collect::<Vec<_>>();
        let found = (numbers[0], numbers[1], numbers[2]);
        if found < REQUIRED_VERSION {
            let (major, minor, patch) = REQUIRED_VERSION;
            bail!(
                "summoner needs grove >= {major}.{minor}.{patch} \
                 (task exec --timeout-secs and structured finish refusals); found {version:?}"
            );
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
    ) -> Result<BeginOutcome> {
        let mut args = vec![
            "task", "begin", "--agent", agent, "--task", title, "--scope",
        ];
        args.extend(scope.iter().map(String::as_str));
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

    #[test]
    fn version_gate_rejects_old_groves() {
        let parse = |v: &str| {
            let numbers = v
                .rsplit(' ')
                .next()
                .unwrap_or_default()
                .split('.')
                .map(|part| part.parse::<u64>().unwrap_or(0))
                .chain(std::iter::repeat(0))
                .take(3)
                .collect::<Vec<_>>();
            (numbers[0], numbers[1], numbers[2])
        };
        assert!(parse("grove 0.3.1") < REQUIRED_VERSION);
        assert!(parse("grove 0.3.2") >= REQUIRED_VERSION);
        assert!(parse("grove 0.4.0") >= REQUIRED_VERSION);
        assert!(parse("grove 1.0.0") >= REQUIRED_VERSION);
    }
}
