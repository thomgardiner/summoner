//! Spawning one executor under the active host's execution plan.

use crate::config::{ExecutorBackend, PromptRouting};
use crate::host::{ExecutionPlan, Host};
use crate::init::CHARTER;
use crate::order::Order;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct ExecRequest<'a> {
    pub host: &'a dyn Host,
    pub backend: &'a ExecutorBackend,
    pub order: &'a Order,
    pub task_id: &'a str,
    pub worktree: &'a Path,
    /// The repository's shared .git directory. Sandboxed executors need it
    /// writable to commit from a linked worktree (git puts the worktree's
    /// index and locks under `<common>/worktrees/<name>/`).
    pub git_common_dir: &'a Path,
    /// Per-order run directory: prompt.md, stdout.log, stderr.log. Never
    /// inside the worktree — an untracked out-of-scope file blocks finish.
    pub run_dir: &'a Path,
    pub timeout_secs: u64,
    pub shutdown: &'a AtomicBool,
    /// The argv template for this spawn. The caller picks the backend's
    /// `argv`, or its `resume_argv` when a revision continues a session.
    pub argv: &'a [String],
    pub resume: bool,
    /// Captured session identifier substituted into `{session_id}`; empty
    /// when no session is being resumed.
    pub session_id: &'a str,
    /// The composed prompt. The caller chooses the charter: worker charter
    /// for implementation, review charter for the quality gate.
    pub prompt: &'a str,
    /// Prefix for this spawn's files in the run dir ("" for the executor,
    /// "review-" for the reviewer, "r2-" for a second attempt), so one
    /// order's runs never collide.
    pub file_prefix: &'a str,
}

pub struct ExecOutcome {
    /// The supervisor's exit code: the executor's own code, 124 (deadline),
    /// or 143 (forwarded termination). None only if the backup kill fired.
    pub exit: Option<i32>,
    pub backup_killed: bool,
}

/// The charter is the contract; the order is the assignment. One document,
/// same for every backend — routing decides how it travels.
pub fn compose_prompt(order: &Order) -> String {
    let mut prompt = String::from(CHARTER);
    prompt.push_str(&format!("\n# Order {}: {}\n", order.id, order.title));
    prompt.push_str("\nScope (the only paths you may change):\n");
    for entry in &order.scope {
        prompt.push_str(&format!("- {entry}\n"));
    }
    prompt.push_str("\nAcceptance criteria (the definition of done):\n");
    if order.acceptance.is_empty() {
        prompt.push_str("- The brief below, plus the automatic verification.\n");
    } else {
        for criterion in &order.acceptance {
            prompt.push_str(&format!("- {criterion}\n"));
        }
    }
    prompt.push_str("\n## Brief\n\n");
    prompt.push_str(&order.brief);
    prompt.push('\n');
    prompt
}

/// Literal per-element substitution: argv stays an array end to end, so
/// vendor greedy-flag orderings and spacing survive exactly as configured.
#[allow(clippy::too_many_arguments)]
pub fn expand(
    template: &[String],
    prompt: &str,
    worktree: &Path,
    git_common_dir: &Path,
    order_file: &Path,
    prompt_file: &Path,
    session_id: &str,
) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            arg.replace("{worktree}", &worktree.display().to_string())
                .replace("{git_common_dir}", &git_common_dir.display().to_string())
                .replace("{order_file}", &order_file.display().to_string())
                .replace("{prompt_file}", &prompt_file.display().to_string())
                .replace("{session_id}", session_id)
                // Last, so placeholder tokens inside the prompt text are never
                // re-scanned: the orchestrator's brief must arrive verbatim.
                .replace("{prompt}", prompt)
        })
        .collect()
}

/// The follow-up prompt for a revision attempt. When the executor's session
/// is resumed, the charter and order are already in its context; a fresh
/// context gets the full assignment again before the evidence.
pub fn compose_revision_prompt(
    order: &Order,
    attempt: u64,
    resumed: bool,
    feedback: &str,
) -> String {
    let mut prompt = if resumed {
        String::new()
    } else {
        compose_prompt(order)
    };
    prompt.push_str(&format!(
        "\n# Revision attempt {attempt} for order {}\n\n",
        order.id
    ));
    prompt.push_str(
        "Your previous attempt is committed on this branch in this worktree, \
         and it was NOT accepted. The evidence:\n\n",
    );
    prompt.push_str(feedback);
    // A forced reflection measurably cuts agents that loop on a dead
    // approach: name the failure before touching the code.
    prompt.push_str(
        "\n\nBefore changing anything, answer for yourself:\n\
         - What exactly failed, in one sentence?\n\
         - What specific change fixes it?\n\
         - Would this repeat the approach that already failed? If so, take a \
         different one.\n\n\
         Then address every point, amend the work on this branch, and commit. \
         The same scope and acceptance criteria apply; verification and \
         review run again.\n",
    );
    prompt
}

pub fn run_executor(req: &ExecRequest) -> Result<ExecOutcome> {
    std::fs::create_dir_all(req.run_dir)
        .with_context(|| format!("creating run dir {}", req.run_dir.display()))?;
    let prompt = req.prompt.to_string();
    // Always on disk, whatever the routing: it is the post-mortem record of
    // exactly what the executor was told.
    let prompt_path = req.run_dir.join(format!("{}prompt.md", req.file_prefix));
    std::fs::write(&prompt_path, &prompt).context("writing prompt.md")?;

    let mut executor_argv = expand(
        req.argv,
        &prompt,
        req.worktree,
        req.git_common_dir,
        &req.order.source,
        &prompt_path,
        req.session_id,
    );
    let expected = if req.resume {
        req.backend.resume_provenance.as_ref()
    } else {
        req.backend.provenance.as_ref()
    }
    .context("executor launch lacks immutable binary provenance")?;
    crate::backend_provenance::require_current(expected, &executor_argv[0], req.worktree)
        .context("validating executor binary immediately before launch")?;
    // Spawn the exact verified binary: bare names cannot start .cmd shims on
    // Windows, and the recorded provenance path is what the run evidence claims.
    executor_argv[0] = expected.resolved_path.clone();
    let plan = req
        .host
        .execution_plan(req.task_id, req.timeout_secs, &executor_argv)?;

    let stdout = File::create(req.run_dir.join(format!("{}stdout.log", req.file_prefix)))
        .context("creating stdout.log")?;
    let stderr = File::create(req.run_dir.join(format!("{}stderr.log", req.file_prefix)))
        .context("creating stderr.log")?;

    match plan {
        ExecutionPlan::SummonerSupervised { argv, timeout_secs } => {
            let stdin_bytes = match req.backend.routing() {
                PromptRouting::Stdin => Some(prompt.as_bytes()),
                _ => None,
            };
            let outcome = crate::host::supervise::run(
                &argv,
                req.worktree,
                timeout_secs,
                stdin_bytes,
                Stdio::from(stdout),
                Stdio::from(stderr),
                req.shutdown,
            )?;
            Ok(ExecOutcome {
                exit: outcome.exit,
                backup_killed: outcome.backup_killed,
            })
        }
        ExecutionPlan::HostWrapped {
            argv,
            backup_grace_secs,
        } => run_host_wrapped(req, &argv, backup_grace_secs, &prompt, stdout, stderr),
    }
}

fn run_host_wrapped(
    req: &ExecRequest,
    argv: &[String],
    backup_grace_secs: u64,
    prompt: &str,
    stdout: File,
    stderr: File,
) -> Result<ExecOutcome> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(req.worktree)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.stdin(match req.backend.routing() {
        PromptRouting::Stdin => Stdio::piped(),
        _ => Stdio::null(),
    });
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", argv[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = prompt.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }
    let backup_deadline = Instant::now()
        + Duration::from_secs(
            req.timeout_secs
                .saturating_add(backup_grace_secs)
                .min(31_536_000),
        );
    let mut terminated = false;
    loop {
        if let Some(status) = child.try_wait().context("waiting for host-wrapped exec")? {
            return Ok(ExecOutcome {
                exit: status.code(),
                backup_killed: false,
            });
        }
        if req.shutdown.load(Ordering::SeqCst) && !terminated {
            terminate_supervisor(&child);
            terminated = true;
        }
        if Instant::now() >= backup_deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = req.host.kill_supervised(req.task_id, req.worktree);
            return Ok(ExecOutcome {
                exit: None,
                backup_killed: true,
            });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(unix)]
fn terminate_supervisor(child: &std::process::Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate_supervisor(_child: &std::process::Child) {}

/// The last `limit` bytes of a log, for the report. Seeks instead of reading
/// the file: a runaway executor's multi-gigabyte log must not be loaded whole.
pub fn tail(path: &Path, limit: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(limit as u64)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes).ok()?;
    // Lossy: the seek may land mid-character; a replacement char beats a panic.
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The whole log, but only if it is at most `limit` bytes; `None` if it is
/// missing or larger. A `tail` cannot serve a structured parser: the value it
/// wants (Claude's cumulative usage) lives inside the terminal array element,
/// so a truncated head is unparseable JSON. The cap bounds the read for a
/// pathologically large transcript (Claude's JSON stays in the low KBs because
/// tool output is not inlined).
pub fn read_capped(path: &Path, limit: usize) -> Option<String> {
    use std::io::Read;
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn order() -> Order {
        Order {
            id: "auth-fix".into(),
            title: "Fix token validation".into(),
            brief: "Do the thing.".into(),
            scope: vec!["crate:auth-core".into(), "src/api.rs".into()],
            acceptance: vec!["tests pass".into()],
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            max_tokens: None,
            base: None,
            branch: None,
            variants: Vec::new(),
            claim_group: None,
            variant_of: None,
            after: Vec::new(),
            source: PathBuf::from("orders/auth-fix.toml"),
        }
    }

    #[test]
    fn prompt_carries_charter_scope_acceptance_and_brief_in_order() {
        let prompt = compose_prompt(&order());
        let charter_at = prompt.find("# Worker charter").unwrap();
        let scope_at = prompt.find("- crate:auth-core").unwrap();
        let acceptance_at = prompt.find("- tests pass").unwrap();
        let brief_at = prompt.find("Do the thing.").unwrap();
        assert!(charter_at < scope_at);
        assert!(scope_at < acceptance_at);
        assert!(acceptance_at < brief_at);
    }

    #[test]
    fn revision_prompts_carry_evidence_reflection_and_charter_only_when_fresh() {
        let feedback = "Reviewer findings:\n- hardcoded value";
        let fresh = compose_revision_prompt(&order(), 2, false, feedback);
        assert!(
            fresh.contains("# Worker charter"),
            "fresh context re-briefs"
        );
        assert!(fresh.contains("hardcoded value"), "{fresh}");
        assert!(fresh.contains("What exactly failed"), "{fresh}");

        let resumed = compose_revision_prompt(&order(), 2, true, feedback);
        assert!(
            !resumed.contains("# Worker charter"),
            "a resumed session already has the charter"
        );
        assert!(resumed.contains("hardcoded value"), "{resumed}");
        assert!(resumed.contains("Revision attempt 2"), "{resumed}");
    }

    #[test]
    fn expansion_is_per_element_and_preserves_ordering() {
        let template: Vec<String> = ["run", "--pure", "{prompt}", "--dir", "{worktree}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = expand(
            &template,
            "PROMPT TEXT",
            Path::new("/wt"),
            Path::new("/repo/.git"),
            Path::new("/orders/a.toml"),
            Path::new("/runs/a/prompt.md"),
            "",
        );
        assert_eq!(argv, ["run", "--pure", "PROMPT TEXT", "--dir", "/wt"]);

        let embedded: Vec<String> = [
            "--prompt-file={prompt_file}",
            "--order={order_file}",
            "roots=[\"{git_common_dir}\"]",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let argv = expand(
            &embedded,
            "unused",
            Path::new("/wt"),
            Path::new("/repo/.git"),
            Path::new("/orders/a.toml"),
            Path::new("/runs/a/prompt.md"),
            "",
        );
        assert_eq!(
            argv,
            [
                "--prompt-file=/runs/a/prompt.md",
                "--order=/orders/a.toml",
                "roots=[\"/repo/.git\"]"
            ]
        );

        // Placeholder-shaped text inside the prompt must survive verbatim,
        // never be substituted by a later pass.
        let template: Vec<String> = vec!["{prompt}".to_string()];
        let argv = expand(
            &template,
            "keep {worktree} and {git_common_dir} literal",
            Path::new("/wt"),
            Path::new("/repo/.git"),
            Path::new("/orders/a.toml"),
            Path::new("/runs/a/prompt.md"),
            "",
        );
        assert_eq!(argv, ["keep {worktree} and {git_common_dir} literal"]);
    }

    #[test]
    fn tail_seeks_to_the_end_and_survives_a_mid_character_cut() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, format!("{}é-tail", "x".repeat(100))).unwrap();
        // The 6-byte window starts inside the two-byte 'é'.
        let cut = tail(&path, 6).unwrap();
        assert!(cut.ends_with("-tail"), "{cut:?}");
        assert!(cut.starts_with('\u{FFFD}'), "{cut:?}");
        assert_eq!(tail(&path, 4).unwrap(), "tail");
    }
}
