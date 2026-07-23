//! The convergent tail of every order plus the shared plumbing under it:
//! finalize/release, diff evidence, git and grove-verify wrappers, log
//! scraping, and the backup process-group kill. Everything here is called
//! from the per-order state machine in `drive`.

use crate::executor;
use crate::grove::VerifySummary;
use crate::order::Order;
use crate::report::{DiffStats, OrderReport, Outcome};
use crate::run::Ctx;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const TAIL_BYTES: usize = 2048;

/// The convergent tail: collect evidence, abandon a non-terminal task, then
/// release (or deliberately keep) the worktree.
pub(crate) fn finalize(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    abandon_reason: Option<&str>,
) {
    // Capture the executor's result before any lifecycle mutation. The
    // internal-error path reaches this function too, so its report must retain
    // committed work and diff evidence even when abandon or release fails.
    if let Some(base) = report.base_commit.clone() {
        report.commits = git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        report.diff = Some(diff_stats(worktree, &base));
    }
    // The last moment the reviewed candidate is identifiable: release may
    // salvage dirty state into a fresh commit and move the branch, after which
    // the branch name no longer names what was verified and reviewed.
    //
    // HEAD is that identity only when the tree is clean. Verification and
    // review accept staged, unstaged, and untracked work, so a dirty tree
    // means the reviewed candidate is HEAD plus a delta no commit names yet.
    // Recording HEAD anyway would hand dependents a tree silently missing that
    // delta — worse than recording nothing, which makes dependents refuse.
    report.candidate_commit = match git(worktree, &["status", "--porcelain"]) {
        Ok(status) if status.trim().is_empty() => git(worktree, &["rev-parse", "HEAD"])
            .ok()
            .map(|oid| oid.trim().to_string())
            .filter(|oid| !oid.is_empty()),
        _ => {
            report.detail = Some(match report.detail.take() {
                Some(detail) => format!(
                    "{detail}; uncommitted work at finish, so no candidate commit was recorded"
                ),
                None => {
                    "uncommitted work at finish, so no candidate commit was recorded".to_string()
                }
            });
            None
        }
    };
    if let Some(path) = &report.stdout_log {
        report.stdout_tail = executor::tail(Path::new(path), TAIL_BYTES);
    }
    if let Some(path) = &report.stderr_log {
        report.stderr_tail = executor::tail(Path::new(path), TAIL_BYTES);
    }

    // A hard kill during cleanup must not erase a completed Grove gate. Resume
    // still requires Grove's durable task status to agree with this checkpoint.
    let _ = ctx.events.emit_report("order_checkpoint", report);

    if let Some(reason) = abandon_reason
        && let Err(error) = ctx.host.task_abandon(worktree, task_id, reason)
    {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; abandon failed: {error:#}"),
            None => format!("abandon failed: {error:#}"),
        });
    }

    let keep = ctx.config.keep_failed_worktrees()
        && !matches!(
            report.outcome,
            Outcome::Verified | Outcome::Completed | Outcome::Approved
        );
    if keep {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; worktree kept for post-mortem"),
            None => "worktree kept for post-mortem".to_string(),
        });
    } else {
        release(ctx, worktree, report);
        // A leaked worktree (or failed salvage) is not a success, whatever the
        // receipts say: dependents must not build on it and the run must not
        // exit 0. Deliberate keep_failed_worktrees is different — that skip is
        // requested, not a failure.
        if report.release_error.is_some()
            && matches!(
                report.outcome,
                Outcome::Verified | Outcome::Completed | Outcome::Approved
            )
        {
            report.outcome = Outcome::Error;
        }
    }
    let _ = order;
}

pub(crate) fn release(ctx: &Ctx, worktree: &Path, report: &mut OrderReport) {
    match ctx.host.worktree_release(&ctx.repo, worktree) {
        Ok(outcome) => {
            report.saved_to = outcome.saved_to;
            if report.branch.is_none() {
                report.branch = outcome.branch;
            }
        }
        // Reap will NOT clean a checkout that left its branch; say so plainly.
        Err(error) => {
            report.release_error = Some(format!("{error:#}; needs manual recovery"));
        }
    }
}

fn diff_stats(worktree: &Path, base: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    if let Ok(shortstat) = git(worktree, &["diff", "--shortstat", &format!("{base}..HEAD")]) {
        for part in shortstat.split(',') {
            let number: u64 = part
                .trim()
                .split(' ')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if part.contains("file") {
                stats.files_changed = number;
            } else if part.contains("insertion") {
                stats.insertions = number;
            } else if part.contains("deletion") {
                stats.deletions = number;
            }
        }
    }
    if let Ok(porcelain) = git(worktree, &["status", "--porcelain"]) {
        stats.uncommitted_files = porcelain.lines().filter(|l| !l.is_empty()).count() as u64;
    }
    stats
}

/// The cumulative prompt-cache split (read, write) from Claude Code's
/// `--output-format json` output, or `None` for any executor whose output is
/// not that envelope (codex prints plain text; nothing to read).
///
/// This cannot be a substring marker like [`number_after`]. Claude repeats the
/// same keys inside `usage.iterations` (one entry per turn) *after* the
/// cumulative top-level copy, so a last-match text scan reports a single turn
/// instead of the run total — measured 24,807 for a turn against a true 66,448.
/// Reading `cache_read_input_tokens` / `cache_creation_input_tokens` as direct
/// children of the terminal `result` message's `usage` is the only correct
/// read. The streaming deserializer tolerates one value (`--output-format
/// json`) or many (`stream-json`); the last `result` seen wins.
pub(crate) fn claude_cache_split(output: &str) -> Option<(u64, u64)> {
    use serde_json::Value;
    fn split_of(message: &Value) -> Option<(u64, u64)> {
        if message.get("type")? != "result" {
            return None;
        }
        let usage = message.get("usage")?;
        let read = usage.get("cache_read_input_tokens")?.as_u64()?;
        let write = usage.get("cache_creation_input_tokens")?.as_u64()?;
        Some((read, write))
    }
    let mut split = None;
    let stream = serde_json::Deserializer::from_str(output).into_iter::<Value>();
    for value in stream.flatten() {
        let messages = match &value {
            Value::Array(items) => items.iter().filter_map(split_of).next_back(),
            message => split_of(message),
        };
        if let Some(found) = messages {
            split = Some(found);
        }
    }
    split
}

/// The first number after the LAST occurrence of `marker`, tolerating comma
/// and underscore separators (codex prints "tokens used\n40,958").
pub(crate) fn number_after(text: &str, marker: &str) -> Option<u64> {
    let rest = &text[text.rfind(marker)? + marker.len()..];
    let start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '_')
        .filter(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn grove_verify(
    ctx: &Ctx<'_>,
    worktree: &Path,
    profile: &str,
    task_id: &str,
) -> Result<VerifySummary> {
    let ignored_before = ignored_paths(worktree)?;
    let verification = ctx.host.verify(worktree, profile, task_id);
    let cleanup = clean_new_ignored_paths(worktree, &ignored_before);
    let summary = verification?;
    cleanup?;
    Ok(summary)
}

fn ignored_paths(worktree: &Path) -> Result<BTreeSet<PathBuf>> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ])
        .current_dir(worktree)
        .output()
        .context("listing ignored worktree paths")?;
    if !output.status.success() {
        bail!(
            "listing ignored worktree paths failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    std::str::from_utf8(&output.stdout)
        .context("ignored worktree path is not UTF-8")?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            {
                Ok(path)
            } else {
                bail!("git returned unsafe ignored path {}", path.display())
            }
        })
        .collect()
}

fn clean_new_ignored_paths(worktree: &Path, before: &BTreeSet<PathBuf>) -> Result<()> {
    for relative in ignored_paths(worktree)?.difference(before) {
        let path = worktree.join(relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting verifier artifact {}", path.display()));
            }
        };
        if metadata.file_type().is_dir() {
            std::fs::remove_dir(&path)
                .with_context(|| format!("removing verifier artifact {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing verifier artifact {}", path.display()))?;
        }
    }
    Ok(())
}

/// First whitespace-delimited token after the LAST occurrence of `marker`.
/// The token is executor-controlled output headed for a resume argv, so it
/// must look like a session identifier and nothing else: alphanumeric start
/// (never an option), a conservative id charset (never a `{placeholder}`
/// that a later substitution pass would re-expand), bounded length.
pub(crate) fn token_after(text: &str, marker: &str) -> Option<String> {
    let rest = &text[text.rfind(marker)? + marker.len()..];
    let token: String = rest
        .trim_start()
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    let id_shaped = !token.is_empty()
        && token.len() <= 128
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    id_shaped.then_some(token)
}

/// The first and last 16 KiB of a log: session banners print early, usage
/// footers late, and a runaway log must never be read whole.
pub(crate) fn head_and_tail(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 16 * 1024];
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    let mut text = String::from_utf8_lossy(&head).into_owned();
    if let Some(tail) = executor::tail(path, 16 * 1024) {
        text.push('\n');
        text.push_str(&tail);
    }
    Some(text)
}

#[cfg(unix)]
pub(crate) fn kill_recorded_group(ctx: &Ctx, task_id: &str, worktree: &Path) {
    let Ok(status) = ctx.host.task_status(worktree) else {
        return;
    };
    let Some(tasks) = status["tasks"].as_array() else {
        return;
    };
    for task in tasks {
        if task["id"] == task_id
            && let Some(pid) = task["active_command"]["pid"].as_u64()
        {
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_recorded_group(_ctx: &Ctx, _task_id: &str, _worktree: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_numbers_parse_after_the_last_marker() {
        assert_eq!(
            number_after("...\ntokens used\n40,958\n", "tokens used"),
            Some(40_958)
        );
        assert_eq!(
            number_after("tokens used: 5\nmore\ntokens used: 1_200", "tokens used"),
            Some(1_200)
        );
        assert_eq!(number_after("no marker here", "tokens used"), None);
        assert_eq!(
            number_after("tokens used but no number", "tokens used"),
            None
        );
    }

    #[test]
    fn claude_cache_split_reads_the_cumulative_not_the_nested_turn() {
        // A faithful slice of `claude --print --output-format json`: an earlier
        // assistant message carries its own usage, and the terminal result's
        // cumulative (66448/8494) is followed in the byte stream by a smaller
        // per-turn copy nested in `iterations` (24807/614). A text scan would
        // report the last, wrong numbers; the parse must report the cumulative.
        let output = r#"[
          {"type":"assistant","message":{"usage":{"cache_read_input_tokens":40870,"cache_creation_input_tokens":7880}}},
          {"type":"result","subtype":"success","usage":{"input_tokens":2,"cache_creation_input_tokens":8494,"cache_read_input_tokens":66448,"output_tokens":4,"iterations":[{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":24807,"cache_creation_input_tokens":614}]}}
        ]"#;
        assert_eq!(claude_cache_split(output), Some((66_448, 8_494)));

        // stream-json: the same values across concatenated line objects; the
        // last `result` wins over an earlier one.
        let stream = concat!(
            r#"{"type":"assistant","message":{"usage":{"cache_read_input_tokens":1}}}"#,
            "\n",
            r#"{"type":"result","usage":{"cache_read_input_tokens":300,"cache_creation_input_tokens":40}}"#,
        );
        assert_eq!(claude_cache_split(stream), Some((300, 40)));

        // Codex prints plain text; there is nothing to parse, so no false split.
        assert_eq!(claude_cache_split("ok\ntokens used\n8,673\n"), None);
        // A result without the cache fields is simply not measured.
        assert_eq!(
            claude_cache_split(r#"{"type":"result","usage":{"input_tokens":5}}"#),
            None
        );
    }

    #[test]
    fn session_tokens_parse_after_the_last_marker() {
        assert_eq!(
            token_after("banner\nsession id: abc-123\nwork...", "session id:"),
            Some("abc-123".into())
        );
        assert_eq!(
            token_after("session id: old\nsession id: new-9", "session id:"),
            Some("new-9".into())
        );
        assert_eq!(token_after("no marker here", "session id:"), None);
        assert_eq!(token_after("session id:   \n", "session id:"), None);
        // Executor-controlled output must not become an option or a
        // placeholder that a later substitution pass would re-expand.
        assert_eq!(token_after("session id: --resume-all", "session id:"), None);
        assert_eq!(token_after("session id: {prompt}", "session id:"), None);
        assert_eq!(token_after("session id: a;rm", "session id:"), None);
        let oversized = format!("session id: {}", "a".repeat(200));
        assert_eq!(token_after(&oversized, "session id:"), None);
    }
}
