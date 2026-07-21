//! The evidence gates an attempt must pass: the order's verification
//! profile, the finish-driven receipt loop, and the independent reviewer.
//! All of it runs while the grove task is live, so the gates and the claim
//! system always agree about who owns the scope.

use crate::executor::{self, ExecRequest};
use crate::grove::FinishOutcome;
use crate::order::Order;
use crate::outcome::{git, grove_verify, kill_recorded_group, number_after};
use crate::report::{OrderReport, Outcome, ReviewSummary};
use crate::review;
use crate::run::{Ctx, SHUTDOWN};
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The order's own verification profile. Returns false when the outcome is
/// already decided (profile failed or the run was interrupted).
pub(crate) fn profile_verify(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    ran: &mut std::collections::BTreeSet<String>,
) -> Result<bool> {
    // An interrupt cannot stop a verify subprocess mid-flight, but it must not
    // start the next one; the convergent tail still abandons and releases.
    if SHUTDOWN.load(Ordering::SeqCst) {
        report.outcome = Outcome::Interrupted;
        report.detail = Some("interrupted before verification".into());
        return Ok(false);
    }
    if let Some(profile) = order
        .verify_profile
        .clone()
        .or_else(|| ctx.config.default_verify_profile.clone())
    {
        let summary = grove_verify(ctx, worktree, &profile, task_id)?;
        let passed = summary.passed;
        ctx.events.emit(
            "order_verify",
            serde_json::json!({"id": order.id, "profile": profile, "passed": passed}),
        )?;
        ran.insert(profile.clone());
        report.verify.push(summary);
        if !passed {
            report.outcome = Outcome::Unverified;
            report.detail = Some(format!("verify profile {profile:?} failed"));
            return Ok(false);
        }
    }
    Ok(true)
}

/// Finish, refusal-driven: attempt it, run exactly the profiles a refusal
/// names, retry once.
pub(crate) fn finish_task(
    ctx: &Ctx,
    order: &Order,
    task_id: &str,
    worktree: &Path,
    report: &mut OrderReport,
    ran: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    let _ = order;
    for attempt in 0..2 {
        if SHUTDOWN.load(Ordering::SeqCst) {
            report.outcome = Outcome::Interrupted;
            report.detail = Some("interrupted during verification".into());
            return Ok(());
        }
        match ctx.grove.task_finish(worktree, task_id, None)? {
            FinishOutcome::Finished { verification } => {
                report.finish = Some(verification);
                report.outcome = Outcome::Verified;
                return Ok(());
            }
            FinishOutcome::Refused {
                reason,
                outside_scope,
                verification,
            } => {
                if reason == "scope" {
                    report.outcome = Outcome::ScopeViolation;
                    report.detail = Some(format!("out of scope: {}", outside_scope.join(", ")));
                    return Ok(());
                }
                // No verification block means grove refused for a reason this
                // version cannot act on; "the repository requires nothing" is
                // only ever an EXPLICIT empty required list.
                let Some(verification) = verification else {
                    report.outcome = Outcome::Unverified;
                    report.detail = Some(format!(
                        "finish refused ({reason}) without verification detail"
                    ));
                    return Ok(());
                };
                let wanted: Vec<String> = verification
                    .missing
                    .iter()
                    .chain(verification.stale.iter())
                    .filter(|profile| !ran.contains(*profile))
                    .cloned()
                    .collect();
                if verification.required.is_empty() {
                    // The repository declares no required profiles; grove can
                    // never mark this verified. Finish with the override on
                    // the record and report it as completed, not verified.
                    let reason = "summoner: repository declares no required verification profiles";
                    if let FinishOutcome::Finished { verification } =
                        ctx.grove.task_finish(worktree, task_id, Some(reason))?
                    {
                        report.finish = Some(verification);
                        report.outcome = Outcome::Completed;
                        report.detail = Some(reason.to_string());
                        return Ok(());
                    }
                    report.outcome = Outcome::Unverified;
                    report.detail = Some("finish refused the explicit override".into());
                    return Ok(());
                }
                if attempt == 1 || wanted.is_empty() {
                    report.finish = Some(verification);
                    report.outcome = Outcome::Unverified;
                    report.detail = Some("finish refused: required evidence not fresh".into());
                    return Ok(());
                }
                for profile in wanted {
                    let summary = grove_verify(ctx, worktree, &profile, task_id)?;
                    let passed = summary.passed;
                    ran.insert(profile.clone());
                    report.verify.push(summary);
                    if !passed {
                        report.outcome = Outcome::Unverified;
                        report.detail = Some(format!("required profile {profile:?} failed"));
                        return Ok(());
                    }
                }
            }
        }
    }
    unreachable!("finish loop returns within two attempts");
}

pub(crate) enum ReviewDecision {
    Approve,
    Reject,
    Failed(String),
    Interrupted,
}

/// The independent quality gate: a fresh reviewer backend spawned under the
/// order's live task, given the requirements and the diff — never the
/// implementer's transcript. Any write it makes is undone and voids its
/// verdict, so an approve can only come from a reviewer that stayed read-only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn review_gate(
    ctx: &Ctx,
    order: &Order,
    reviewer: &str,
    task_id: &str,
    worktree: &Path,
    git_common_dir: &Path,
    order_dir: &Path,
    base: &str,
    prefix: &str,
    report: &mut OrderReport,
) -> Result<ReviewDecision> {
    let backend = &ctx.config.executors[reviewer];
    let timeout_secs = backend
        .timeout_secs
        .unwrap_or_else(|| ctx.config.order_timeout_secs());
    // The live delta, not base..HEAD: verification ran against this tree, so
    // the reviewer must judge everything in it — staged, unstaged, and (via
    // the status listing) untracked. A diff the gate cannot collect is an
    // error, never an empty diff silently approved.
    let diff = git(worktree, &["diff", base]).context("collecting the review diff")?;
    let diff_stat =
        git(worktree, &["diff", "--stat", base]).context("collecting the review diff stat")?;
    let uncommitted = git(worktree, &["status", "--porcelain"])
        .context("collecting the review status listing")?;
    let prompt = review::compose_prompt(
        order,
        base,
        &report.tripwires,
        &report.verify,
        &diff,
        &diff_stat,
        &uncommitted,
    );
    let before = review::snapshot(worktree)?;
    // Attempt-scoped names so a revision's review never clobbers the last.
    let review_prefix = format!("{prefix}review-");
    let stdout_log = order_dir.join(format!("{review_prefix}stdout.log"));
    let stderr_log = order_dir.join(format!("{review_prefix}stderr.log"));
    // Reviews run for minutes; a live consumer needs the logs to tail the
    // moment the reviewer spawns, not a verdict event after the fact.
    ctx.events.emit(
        "review_started",
        serde_json::json!({
            "id": order.id,
            "reviewer": reviewer,
            "stdout_log": stdout_log.display().to_string(),
            "stderr_log": stderr_log.display().to_string(),
            "timeout_secs": timeout_secs,
        }),
    )?;
    let started = Instant::now();
    let exec = executor::run_executor(&ExecRequest {
        grove: &ctx.grove,
        backend,
        order,
        task_id,
        worktree,
        git_common_dir,
        run_dir: order_dir,
        timeout_secs,
        shutdown: &SHUTDOWN,
        argv: &backend.argv,
        session_id: "",
        prompt: &prompt,
        file_prefix: &review_prefix,
    })?;
    let mut summary = ReviewSummary {
        reviewer: reviewer.to_string(),
        verdict: "failed".into(),
        detail: None,
        findings: Vec::new(),
        exit: exec.exit,
        duration_secs: started.elapsed().as_secs(),
        stdout_log: Some(stdout_log.display().to_string()),
        stderr_log: Some(stderr_log.display().to_string()),
    };
    if let Some(marker) = &backend.usage_marker
        && let Some(extra) = [&stderr_log, &stdout_log].iter().find_map(|path| {
            executor::tail(path, 8192)
                .as_deref()
                .and_then(|text| number_after(text, marker))
        })
    {
        report.usage_tokens = Some(report.usage_tokens.unwrap_or(0).saturating_add(extra));
        ctx.spent.fetch_add(extra, Ordering::SeqCst);
    }
    // A wedged supervisor can leave the reviewer's group alive and still
    // writing; kill it BEFORE undoing worktree state, or the restoration
    // races the very process it is cleaning up after.
    if exec.backup_killed {
        kill_recorded_group(ctx, task_id, worktree);
    }
    let violations = review::restore(worktree, &before)?;

    let decision = if exec.backup_killed {
        summary.detail = Some("review supervisor did not return; backup deadline fired".into());
        ReviewDecision::Failed("review failed: supervisor wedged".into())
    } else if SHUTDOWN.load(Ordering::SeqCst) {
        summary.detail = Some("interrupted by operator".into());
        ReviewDecision::Interrupted
    } else if !violations.is_empty() {
        summary.detail = Some(format!(
            "reviewer modified the worktree (undone): {}",
            violations.join(", ")
        ));
        ReviewDecision::Failed("review failed: reviewer modified the worktree".into())
    } else if exec.exit == Some(124) {
        summary.detail = Some("review timed out".into());
        ReviewDecision::Failed("review failed: timeout".into())
    } else if exec.exit != Some(0) {
        summary.detail = Some(format!("reviewer exited {:?}", exec.exit));
        ReviewDecision::Failed(format!("review failed: reviewer exited {:?}", exec.exit))
    } else {
        match executor::tail(&stdout_log, 64 * 1024)
            .as_deref()
            .and_then(review::parse_verdict)
        {
            Some(parsed) => {
                summary.findings = parsed.findings;
                match parsed.verdict {
                    review::Verdict::Approve => {
                        summary.verdict = "approve".into();
                        ReviewDecision::Approve
                    }
                    review::Verdict::Reject => {
                        summary.verdict = "reject".into();
                        ReviewDecision::Reject
                    }
                }
            }
            None => {
                summary.detail = Some("no verdict JSON in reviewer output".into());
                ReviewDecision::Failed("review failed: no verdict in output".into())
            }
        }
    };
    ctx.events.emit(
        "order_review",
        serde_json::json!({
            "id": order.id,
            "reviewer": reviewer,
            "verdict": summary.verdict,
            "findings": summary.findings.len(),
            "detail": summary.detail,
        }),
    )?;
    report.review = Some(summary);
    Ok(decision)
}
