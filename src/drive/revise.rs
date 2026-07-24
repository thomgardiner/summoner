//! Revision loop helpers and worker-result parsing.

use crate::order::Order;
use crate::report::{OrderReport, Outcome};
use crate::run::{Ctx, SHUTDOWN};
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::Ordering;

pub(crate) fn revision_viable(
    ctx: &Ctx,
    order: &Order,
    report: &mut OrderReport,
    max_attempts: u64,
) -> Option<&'static str> {
    let note = |report: &mut OrderReport, text: &str| {
        report.detail = Some(match report.detail.take() {
            Some(detail) => format!("{detail}; {text}"),
            None => text.to_string(),
        });
    };
    if report.attempts >= max_attempts {
        return None;
    }
    if SHUTDOWN.load(Ordering::SeqCst) {
        note(report, "shutdown before the revision could dispatch");
        return None;
    }
    // A cap means "spend no more once reached", so equality blocks too.
    if let (Some(cap), Some(used)) = (order.max_tokens, report.usage_tokens)
        && used >= cap
    {
        note(
            report,
            &format!("order token budget reached ({used} of {cap}) — not revised"),
        );
        return None;
    }
    if let Some(budget) = ctx.config.run_token_budget() {
        let spent = ctx.spent.load(Ordering::SeqCst);
        if spent >= budget {
            note(
                report,
                &format!("run token budget exhausted ({spent} of {budget}) — not revised"),
            );
            return None;
        }
    }
    Some(match report.outcome {
        Outcome::Rejected => "rejected",
        _ => "unverified",
    })
}

/// The evidence the next attempt must address: reviewer findings when the
/// gate rejected, the verification failure otherwise.
pub(crate) fn revision_feedback(report: &OrderReport) -> String {
    if let Some(review) = &report.review
        && review.verdict == "reject"
    {
        format!(
            "Reviewer findings:\n{}",
            serde_json::to_string_pretty(&review.findings).unwrap_or_default()
        )
    } else {
        format!(
            "Verification failure: {}",
            report.detail.as_deref().unwrap_or("unspecified")
        )
    }
}

/// Reset the report to a clean slate for the next attempt. Everything an
/// attempt produced is cleared — a second attempt failing before review must
/// not report the first attempt's verdict, and a later reviewer must not see
/// receipts from a superseded attempt. (Callers compute the revision
/// feedback from this state BEFORE calling.)
pub(crate) fn revise(
    ctx: &Ctx,
    order: &Order,
    order_dir: &Path,
    report: &mut OrderReport,
    reason: &str,
) -> Result<()> {
    report.attempts += 1;
    report.detail = None;
    report.finish = None;
    report.review = None;
    report.verify.clear();
    report.tripwires.clear();
    report.executor_exit = None;
    let prefix = format!("r{}-", report.attempts);
    ctx.events.emit(
        "order_revised",
        serde_json::json!({
            "id": order.id,
            "attempt": report.attempts,
            "reason": reason,
            "task_id": report.task_id,
            // The next attempt's logs, so a live consumer can keep tailing.
            "stdout_log": order_dir.join(format!("{prefix}stdout.log")).display().to_string(),
            "stderr_log": order_dir.join(format!("{prefix}stderr.log")).display().to_string(),
        }),
    )
}

const WORKER_RESULT_WINDOW: usize = 10;

pub(crate) fn parse_worker_result(output: &str) -> Option<Result<(), String>> {
    for line in output
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(WORKER_RESULT_WINDOW)
    {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(status) = value.get("summoner_status") else {
            continue;
        };
        let Some(status) = status.as_str() else {
            return Some(Err(
                "executor emitted a malformed summoner completion result".into(),
            ));
        };
        let Some(unmet) = value.get("unmet").and_then(serde_json::Value::as_array) else {
            return Some(Err(
                "executor completion result is missing an unmet list".into()
            ));
        };
        let Some(unmet) = unmet
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<_>>>()
        else {
            return Some(Err(
                "executor completion result has a malformed unmet list".into()
            ));
        };
        return Some(match (status, unmet.is_empty()) {
            ("complete", true) => Ok(()),
            ("complete", false) | ("incomplete", false) => Err(format!(
                "executor reported unmet acceptance: {}",
                unmet.join("; ")
            )),
            ("incomplete", true) => Err("executor reported incomplete work".into()),
            _ => Err(format!(
                "executor reported unknown summoner status {status:?}"
            )),
        });
    }
    None
}

#[cfg(test)]
mod worker_result_tests {
    use super::parse_worker_result;

    #[test]
    fn complete_requires_an_empty_unmet_list() {
        assert!(matches!(
            parse_worker_result(r#"{"summoner_status":"complete","unmet":[]}"#),
            Some(Ok(()))
        ));
        assert!(matches!(
            parse_worker_result(
                r#"{"summoner_status":"complete","unmet":["wire test"]}"#
            ),
            Some(Err(reason)) if reason.contains("wire test")
        ));
    }

    #[test]
    fn incomplete_and_malformed_results_fail_closed() {
        assert!(matches!(
            parse_worker_result(
                r#"{"summoner_status":"incomplete","unmet":["Windows artifact"]}"#
            ),
            Some(Err(reason)) if reason.contains("Windows artifact")
        ));
        assert!(matches!(
            parse_worker_result(r#"{"summoner_status":"complete"}"#),
            Some(Err(_))
        ));
    }

    #[test]
    fn only_the_trailing_window_is_trusted() {
        let buried = format!(
            "{}\n{}",
            r#"{"summoner_status":"complete","unmet":[]}"#,
            (0..11).map(|_| "footer").collect::<Vec<_>>().join("\n")
        );
        assert!(parse_worker_result(&buried).is_none());
    }
}

