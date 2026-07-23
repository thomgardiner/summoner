//! The evidence gates an attempt must pass: the order's verification
//! profile, the finish-driven receipt loop, and the independent reviewer.
//! All of it runs while the grove task is live, so the gates and the claim
//! system always agree about who owns the scope.

use crate::grove::FinishOutcome;
use crate::order::Order;
use crate::outcome::grove_verify;
use crate::report::{OrderReport, Outcome};
use crate::run::{Ctx, SHUTDOWN};
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::Ordering;

pub(crate) use crate::review_gate::{ReviewDecision, run as review_gate};

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
    let mut profiles = Vec::new();
    if let Some(profile) = order
        .verify_profile
        .clone()
        .or_else(|| ctx.config.default_verify_profile.clone())
    {
        profiles.push(profile);
    }
    if let Some(policy) = ctx.config.trusted_policy.as_ref() {
        for profile in &policy.required_profiles {
            if !profiles.iter().any(|p| p == profile) {
                profiles.push(profile.clone());
            }
        }
    }
    for profile in profiles {
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
    expected_source_sha256: Option<&str>,
) -> Result<()> {
    let _ = order;
    for attempt in 0..2 {
        if SHUTDOWN.load(Ordering::SeqCst) {
            report.outcome = Outcome::Interrupted;
            report.detail = Some("interrupted during verification".into());
            return Ok(());
        }
        match ctx
            .host
            .task_finish(worktree, task_id, None, expected_source_sha256)?
        {
            FinishOutcome::Finished { verification } => {
                // Hosts must set verification.verified only when required
                // profiles actually ran and passed. Empty requirements →
                // completed (honest), not verified.
                report.outcome = if verification.verified {
                    Outcome::Verified
                } else {
                    Outcome::Completed
                };
                if !verification.verified {
                    report.detail = Some(
                        "finished without required verification profiles; outcome is completed, not verified"
                            .into(),
                    );
                }
                report.finish = Some(verification);
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
                if reason == "source_changed" {
                    report.outcome = Outcome::Unverified;
                    report.detail = Some(
                        "candidate changed after review; the bound verdict was not applied".into(),
                    );
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
                    if let FinishOutcome::Finished { verification } = ctx.host.task_finish(
                        worktree,
                        task_id,
                        Some(reason),
                        expected_source_sha256,
                    )? {
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
