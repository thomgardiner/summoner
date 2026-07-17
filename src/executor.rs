//! Spawning one executor under grove's supervision. Summoner does no process
//! group management: `grove task exec --timeout-secs` owns the deadline and
//! forwards termination signals to the executor's group, so the fleet is
//! bounded even if summoner itself dies. What lives here is prompt
//! composition, argv template expansion, stdio wiring, and a backup wait.

use crate::config::{ExecutorBackend, PromptRouting};
use crate::grove::GroveCli;
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
    pub grove: &'a GroveCli,
    pub backend: &'a ExecutorBackend,
    pub order: &'a Order,
    pub task_id: &'a str,
    pub worktree: &'a Path,
    /// Per-order run directory: prompt.md, stdout.log, stderr.log. Never
    /// inside the worktree — an untracked out-of-scope file blocks finish.
    pub run_dir: &'a Path,
    pub timeout_secs: u64,
    pub shutdown: &'a AtomicBool,
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
pub fn expand(
    template: &[String],
    prompt: &str,
    worktree: &Path,
    order_file: &Path,
    prompt_file: &Path,
) -> Vec<String> {
    template
        .iter()
        .map(|arg| {
            arg.replace("{prompt}", prompt)
                .replace("{worktree}", &worktree.display().to_string())
                .replace("{order_file}", &order_file.display().to_string())
                .replace("{prompt_file}", &prompt_file.display().to_string())
        })
        .collect()
}

pub fn run_executor(req: &ExecRequest) -> Result<ExecOutcome> {
    std::fs::create_dir_all(req.run_dir)
        .with_context(|| format!("creating run dir {}", req.run_dir.display()))?;
    let prompt = compose_prompt(req.order);
    // Always on disk, whatever the routing: it is the post-mortem record of
    // exactly what the executor was told.
    let prompt_path = req.run_dir.join("prompt.md");
    std::fs::write(&prompt_path, &prompt).context("writing prompt.md")?;

    let executor_argv = expand(
        &req.backend.argv,
        &prompt,
        req.worktree,
        &req.order.source,
        &prompt_path,
    );
    let argv = req
        .grove
        .exec_argv(req.task_id, req.timeout_secs, &executor_argv);

    let stdout = File::create(req.run_dir.join("stdout.log")).context("creating stdout.log")?;
    let stderr = File::create(req.run_dir.join("stderr.log")).context("creating stderr.log")?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(req.worktree)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    command.stdin(match req.backend.prompt {
        // grove task exec passes stdin through untouched to the executor.
        PromptRouting::Stdin => Stdio::piped(),
        // Closed stdin so headless CLIs cannot hang waiting for input.
        _ => Stdio::null(),
    });
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", argv[0]))?;

    // A writer thread: a prompt larger than the pipe buffer must not deadlock
    // against a child that is still starting up.
    let stdin_writer = child.stdin.take().map(|mut stdin| {
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        })
    });

    // grove owns the real deadline; this fires only if the supervisor itself
    // is broken or wedged.
    let backup_deadline = Instant::now() + Duration::from_secs(req.timeout_secs + 30);
    let mut terminated = false;
    let outcome = loop {
        if let Some(status) = child.try_wait().context("waiting for grove task exec")? {
            break ExecOutcome {
                exit: status.code(),
                backup_killed: false,
            };
        }
        if req.shutdown.load(Ordering::SeqCst) && !terminated {
            // TERM the supervisor; it forwards to the executor's group,
            // records the interruption, and exits 143.
            terminate_supervisor(&child);
            terminated = true;
        }
        if Instant::now() >= backup_deadline {
            let _ = child.kill();
            let _ = child.wait();
            break ExecOutcome {
                exit: None,
                backup_killed: true,
            };
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if let Some(writer) = stdin_writer {
        let _ = writer.join();
    }
    Ok(outcome)
}

#[cfg(unix)]
fn terminate_supervisor(child: &std::process::Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn terminate_supervisor(_child: &std::process::Child) {}

/// The last `limit` bytes of a log, for the report.
pub fn tail(path: &Path, limit: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let start = text.len().saturating_sub(limit);
    // Snap to a char boundary so a multibyte character at the cut cannot panic.
    let mut start = start;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(text[start..].to_string())
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
            timeout_secs: None,
            base: None,
            branch: None,
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
    fn expansion_is_per_element_and_preserves_ordering() {
        let template: Vec<String> = ["run", "--pure", "{prompt}", "--dir", "{worktree}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = expand(
            &template,
            "PROMPT TEXT",
            Path::new("/wt"),
            Path::new("/orders/a.toml"),
            Path::new("/runs/a/prompt.md"),
        );
        assert_eq!(argv, ["run", "--pure", "PROMPT TEXT", "--dir", "/wt"]);

        let embedded: Vec<String> = ["--prompt-file={prompt_file}", "--order={order_file}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let argv = expand(
            &embedded,
            "unused",
            Path::new("/wt"),
            Path::new("/orders/a.toml"),
            Path::new("/runs/a/prompt.md"),
        );
        assert_eq!(
            argv,
            ["--prompt-file=/runs/a/prompt.md", "--order=/orders/a.toml"]
        );
    }

    #[test]
    fn tail_returns_the_last_bytes_on_char_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, format!("{}é-tail", "x".repeat(100))).unwrap();
        let tail = tail(&path, 6).unwrap();
        assert!(tail.ends_with("-tail"));
        assert!(tail.len() <= 6);
    }
}
