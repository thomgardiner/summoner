//! The independent review gate. A reviewer is any configured executor spawned
//! fresh under the order's still-live grove task, prompted with the review
//! charter, the order's requirements, and the diff — and deliberately NOT the
//! implementing executor's logs or reasoning, which would poison its
//! independence. The verdict travels as the last JSON line of its output.

use crate::grove::VerifySummary;
use crate::init::REVIEW_CHARTER;
use crate::order::Order;
use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// Diff bigger than this is summarized to `--stat` in the prompt; the
/// reviewer runs in the worktree and is told how to read the rest itself.
const DIFF_INLINE_CAP: usize = 96 * 1024;

pub enum Verdict {
    Approve,
    Reject,
}

pub struct ParsedReview {
    pub verdict: Verdict,
    pub findings: Vec<serde_json::Value>,
}

/// The last line of the reviewer's output that is a JSON object with a
/// recognizable verdict. Scanning backwards tolerates CLI banners, progress
/// noise, and reviewers that narrate before concluding.
pub fn parse_verdict(output: &str) -> Option<ParsedReview> {
    for line in output.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let verdict = match value["verdict"].as_str() {
            Some("approve") => Verdict::Approve,
            Some("reject") => Verdict::Reject,
            _ => continue,
        };
        let findings = value["findings"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(50)
            .collect();
        return Some(ParsedReview { verdict, findings });
    }
    None
}

/// Charter, then the order's requirements, then machine evidence, then the
/// diff. The implementing executor's transcript is deliberately absent.
pub fn compose_prompt(
    order: &Order,
    base: &str,
    tripwires: &[String],
    verify: &[VerifySummary],
    diff: &str,
    diff_stat: &str,
) -> String {
    let mut prompt = String::from(REVIEW_CHARTER);
    prompt.push_str(&format!("\n# Order {}: {}\n", order.id, order.title));
    prompt.push_str("\nScope the implementer was allowed to change:\n");
    for entry in &order.scope {
        prompt.push_str(&format!("- {entry}\n"));
    }
    prompt.push_str("\nAcceptance criteria (the definition of done):\n");
    if order.acceptance.is_empty() {
        prompt.push_str("- The brief below.\n");
    } else {
        for criterion in &order.acceptance {
            prompt.push_str(&format!("- {criterion}\n"));
        }
    }
    prompt.push_str("\n## Brief given to the implementer\n\n");
    prompt.push_str(&order.brief);
    prompt.push('\n');

    prompt.push_str("\n## Verification evidence\n");
    if verify.is_empty() {
        prompt.push_str("- no verification profile ran\n");
    }
    for summary in verify {
        prompt.push_str(&format!(
            "- profile {:?}: {}\n",
            summary.profile,
            if summary.passed { "passed" } else { "FAILED" }
        ));
    }

    prompt.push_str("\n## Tripwires (deterministic diff scan)\n");
    if tripwires.is_empty() {
        prompt.push_str("- none\n");
    }
    for flag in tripwires {
        prompt.push_str(&format!("- {flag}\n"));
    }

    prompt.push_str(&format!("\n## Diff since base {base}\n\n"));
    if diff.len() <= DIFF_INLINE_CAP {
        prompt.push_str("```diff\n");
        prompt.push_str(diff);
        prompt.push_str("```\n");
    } else {
        prompt.push_str(&format!(
            "The full diff is {} bytes; the summary is below. You are in the \
             worktree — read the rest with `git diff {base}`.\n\n{diff_stat}\n",
            diff.len()
        ));
    }
    prompt
}

/// What the worktree looked like before the reviewer ran: HEAD plus the
/// porcelain status set. Anything new afterwards is the reviewer's doing.
pub struct TreeSnapshot {
    head: String,
    status: BTreeSet<String>,
}

pub fn snapshot(worktree: &Path) -> Result<TreeSnapshot> {
    Ok(TreeSnapshot {
        head: git(worktree, &["rev-parse", "HEAD"])?,
        status: porcelain(worktree)?,
    })
}

/// Detect and undo reviewer writes so the executor's state reaches `task
/// finish` untouched. Returns the offending entries (empty = clean review).
pub fn restore(worktree: &Path, before: &TreeSnapshot) -> Result<Vec<String>> {
    let mut violations = Vec::new();
    let head_now = git(worktree, &["rev-parse", "HEAD"])?;
    if head_now != before.head {
        violations.push(format!("HEAD moved to {head_now}"));
        git(worktree, &["reset", "--hard", &before.head])?;
    }
    for entry in porcelain(worktree)?.difference(&before.status) {
        violations.push(entry.clone());
        let path = entry[2..].trim();
        if entry.starts_with("??") {
            // New untracked file or directory: the reviewer created it.
            let target = worktree.join(path.trim_end_matches('/'));
            if target.is_dir() {
                let _ = std::fs::remove_dir_all(&target);
            } else {
                let _ = std::fs::remove_file(&target);
            }
        } else {
            let _ = git(worktree, &["checkout", "--", path]);
        }
    }
    Ok(violations)
}

fn porcelain(worktree: &Path) -> Result<BTreeSet<String>> {
    Ok(git(worktree, &["status", "--porcelain"])?
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .context("running git")?;
    if !output.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn verdict_is_the_last_json_object_and_tolerates_noise() {
        let output = "\
banner v1.2
thinking about {braces} in prose
{\"verdict\":\"reject\",\"findings\":[]}
more narration
{\"verdict\":\"approve\",\"findings\":[{\"severity\":\"minor\",\"file\":\"a.rs\",\"line\":3,\"summary\":\"nit\"}]}
";
        let parsed = parse_verdict(output).expect("verdict parses");
        assert!(matches!(parsed.verdict, Verdict::Approve));
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0]["severity"], "minor");

        assert!(parse_verdict("no verdict here").is_none());
        assert!(parse_verdict("{\"verdict\":\"maybe\"}").is_none());
        assert!(parse_verdict("{not json").is_none());
    }

    #[test]
    fn review_prompt_carries_charter_order_evidence_and_diff_in_order() {
        let order = Order {
            id: "auth-fix".into(),
            title: "Fix token validation".into(),
            brief: "Do the thing.".into(),
            scope: vec!["src".into()],
            acceptance: vec!["tests pass".into()],
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            base: None,
            branch: None,
            variants: Vec::new(),
            claim_group: None,
            variant_of: None,
            after: Vec::new(),
            source: PathBuf::from("a.toml"),
        };
        let prompt = compose_prompt(
            &order,
            "abc123",
            &["net assertion loss: 2".into()],
            &[],
            "+pub fn wave() {}\n",
            "1 file changed",
        );
        let charter_at = prompt.find("# Review charter").unwrap();
        let brief_at = prompt.find("Do the thing.").unwrap();
        let trip_at = prompt.find("net assertion loss: 2").unwrap();
        let diff_at = prompt.find("+pub fn wave()").unwrap();
        assert!(charter_at < brief_at && brief_at < trip_at && trip_at < diff_at);
        assert!(prompt.contains("no verification profile ran"));

        // Oversized diffs collapse to the stat plus instructions.
        let big = "x".repeat(DIFF_INLINE_CAP + 1);
        let prompt = compose_prompt(&order, "abc123", &[], &[], &big, "9 files changed");
        assert!(!prompt.contains(&big));
        assert!(prompt.contains("git diff abc123"));
        assert!(prompt.contains("9 files changed"));
    }
}
