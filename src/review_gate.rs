use crate::config::PromptRouting;
use crate::executor;
use crate::order::Order;
use crate::outcome::{git, number_after};
use crate::report::{OrderReport, ReviewSummary};
use crate::review::{self, Evidence};
use crate::review_protocol::{self, Binding, Verdict};
use crate::run::{Ctx, SHUTDOWN};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(crate) enum ReviewDecision {
    Approve(String),
    Reject(String),
    Failed(String),
    Interrupted,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    ctx: &Ctx,
    order: &Order,
    reviewer: &str,
    task_id: &str,
    worktree: &Path,
    _git_common_dir: &Path,
    order_dir: &Path,
    base: &str,
    prefix: &str,
    report: &mut OrderReport,
) -> Result<ReviewDecision> {
    if SHUTDOWN.load(Ordering::SeqCst) {
        return Ok(ReviewDecision::Interrupted);
    }
    let backend = &ctx.config.executors[reviewer];
    if backend
        .argv
        .iter()
        .any(|arg| arg.contains("{git_common_dir}"))
    {
        bail!("reviewer {reviewer:?} requests the authoritative Git directory")
    }
    let timeout = backend
        .timeout_secs
        .unwrap_or_else(|| ctx.config.order_timeout_secs());
    let lease = timeout.saturating_add(120).clamp(1, 86_400);
    let acquired = ctx.grove.inspection_acquire(worktree, task_id, lease)?;
    if acquired.schema_version != 1 || acquired.task_id != task_id {
        let _ = ctx.grove.inspection_release(worktree, &acquired.capsule_id);
        bail!("Grove returned an incompatible inspection binding")
    }
    let result = (|| -> Result<ReviewDecision> {
        let diff = git(&acquired.path, &["diff", base])
            .context("collecting review diff from inspection capsule")?;
        let stat = git(&acquired.path, &["diff", "--stat", base])
            .context("collecting review diff stat from inspection capsule")?;
        let status = git(&acquired.path, &["status", "--porcelain"])
            .context("collecting review status from inspection capsule")?;
        let diff_sha256 = review::sha256(diff.as_bytes());
        let binding = Binding::new(acquired.source_sha256.clone(), diff_sha256, reviewer)?;
        let policy_sha256 = ctx
            .config
            .trusted_policy
            .as_ref()
            .map(|policy| policy.sha256());
        let evidence = Evidence {
            base,
            diff: &diff,
            diff_stat: &stat,
            uncommitted: &status,
            tripwires: &report.tripwires,
            verify: &report.verify,
            trusted_policy_sha256: policy_sha256.as_deref(),
        };
        let prompt = review::compose_prompt(order, &evidence, &binding.instructions());
        let review_prefix = format!("{prefix}review-");
        let prompt_path = order_dir.join(format!("{review_prefix}prompt.md"));
        std::fs::write(&prompt_path, prompt.as_bytes()).context("writing review prompt")?;
        let stdout_log = order_dir.join(format!("{review_prefix}stdout.log"));
        let stderr_log = order_dir.join(format!("{review_prefix}stderr.log"));
        ctx.events.emit(
            "review_started",
            serde_json::json!({"id": order.id,
        "reviewer": reviewer, "capsule_id": acquired.capsule_id,
        "review_nonce": binding.nonce,
        "candidate_snapshot_sha256": binding.candidate_snapshot_sha256,
        "diff_sha256": binding.diff_sha256,
        "stdout_log": stdout_log, "stderr_log": stderr_log, "timeout_secs": timeout}),
        )?;
        let argv = reviewer_argv(backend, order, &prompt, &prompt_path, &acquired.path)?;
        let started = Instant::now();
        let execution = ctx
            .grove
            .inspection_exec(worktree, &acquired.capsule_id, timeout, &argv);
        match execution {
            Ok(exec) => evaluate(
                ctx,
                reviewer,
                &binding,
                &acquired.capsule_id,
                exec,
                &stdout_log,
                &stderr_log,
                started,
                backend.usage_marker.as_deref(),
                report,
            ),
            Err(error) => Err(error),
        }
    })();
    let released = ctx.grove.inspection_release(worktree, &acquired.capsule_id);
    match (result, released) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error).context("releasing inspection capsule"),
        (Err(error), Err(release)) => {
            Err(error).context(format!("also failed to release capsule: {release:#}"))
        }
    }
}

fn reviewer_argv(
    backend: &crate::config::ExecutorBackend,
    order: &Order,
    prompt: &str,
    prompt_path: &Path,
    capsule: &Path,
) -> Result<Vec<String>> {
    if backend.routing() == PromptRouting::File {
        bail!(
            "file-routed reviewers are unsupported because the prompt is outside the sealed capsule"
        )
    }
    let expanded = executor::expand(
        &backend.argv,
        prompt,
        capsule,
        Path::new(""),
        &order.source,
        prompt_path,
        "",
    );
    let provenance = backend
        .provenance
        .as_ref()
        .context("reviewer launch lacks immutable binary provenance")?;
    let mut argv = vec![
        std::env::current_exe()?.display().to_string(),
        "__review-worker".into(),
        "--prompt-file".into(),
        prompt_path.display().to_string(),
        "--expected-path".into(),
        provenance.resolved_path.clone(),
        "--expected-sha256".into(),
        provenance.binary_sha256.clone(),
        "--expected-prompt-sha256".into(),
        review::sha256(prompt.as_bytes()),
    ];
    if backend.routing() == PromptRouting::Stdin {
        argv.push("--stdin".into());
    }
    argv.push("--".into());
    argv.extend(expanded);
    Ok(argv)
}

#[allow(clippy::too_many_arguments)]
fn evaluate(
    ctx: &Ctx,
    reviewer: &str,
    binding: &Binding,
    capsule_id: &str,
    exec: crate::grove::InspectionExec,
    stdout_log: &Path,
    stderr_log: &Path,
    started: Instant,
    usage_marker: Option<&str>,
    report: &mut OrderReport,
) -> Result<ReviewDecision> {
    std::fs::copy(&exec.stdout.path, stdout_log).context("preserving raw review stdout")?;
    std::fs::copy(&exec.stderr.path, stderr_log).context("preserving raw review stderr")?;
    let raw = std::fs::read(stdout_log).context("reading review verdict")?;
    let raw_sha = review::sha256(&raw);
    let stderr_raw = std::fs::read(stderr_log).context("reading review stderr")?;
    let stderr_sha = review::sha256(&stderr_raw);
    let mut summary = ReviewSummary {
        reviewer: reviewer.into(),
        verdict: "failed".into(),
        detail: None,
        findings: Vec::new(),
        exit: Some(exec.exit_code),
        duration_secs: started.elapsed().as_secs(),
        stdout_log: Some(stdout_log.display().to_string()),
        stderr_log: Some(stderr_log.display().to_string()),
        protocol_version: review_protocol::VERSION,
        review_nonce: binding.nonce.clone(),
        candidate_snapshot_sha256: binding.candidate_snapshot_sha256.clone(),
        diff_sha256: binding.diff_sha256.clone(),
        raw_stdout_sha256: raw_sha.clone(),
        capsule_id: capsule_id.into(),
    };
    if let Some(marker) = usage_marker
        && let Some(extra) = [stderr_log, stdout_log].iter().find_map(|path| {
            executor::tail(path, 8192)
                .as_deref()
                .and_then(|text| number_after(text, marker))
        })
    {
        report.usage_tokens = Some(report.usage_tokens.unwrap_or(0).saturating_add(extra));
        ctx.spent.fetch_add(extra, Ordering::SeqCst);
    }
    let integrity = exec.schema_version == 1
        && exec.capsule_id == capsule_id
        && exec.task_id == report.task_id.as_deref().unwrap_or("")
        && exec.tree_clean
        && exec.source_unchanged
        && exec.capsule_unchanged
        && exec.authorized
        && exec.source_sha256 == binding.candidate_snapshot_sha256
        && exec.stdout.sha256 == raw_sha
        && exec.stderr.sha256 == stderr_sha
        && exec.stdout.bytes == raw.len() as u64
        && exec.stderr.bytes == stderr_raw.len() as u64
        && !exec.timed_out
        && exec.exit_code == 0;
    let decision = if !integrity {
        summary.detail =
            Some("Grove inspection did not authorize an unchanged candidate and capsule".into());
        ReviewDecision::Failed("review failed: inspection integrity check failed".into())
    } else {
        match review_protocol::parse(&raw, binding) {
            Ok(envelope) => {
                summary.findings = envelope
                    .findings
                    .iter()
                    .map(|item| serde_json::to_value(item).expect("serializable finding"))
                    .collect();
                match envelope.verdict {
                    Verdict::Approve => {
                        summary.verdict = "approve".into();
                        ReviewDecision::Approve(binding.candidate_snapshot_sha256.clone())
                    }
                    Verdict::Reject => {
                        summary.verdict = "reject".into();
                        ReviewDecision::Reject(binding.candidate_snapshot_sha256.clone())
                    }
                }
            }
            Err(error) => {
                summary.detail = Some(format!("invalid bound verdict: {error:#}"));
                ReviewDecision::Failed("review failed: invalid bound verdict".into())
            }
        }
    };
    ctx.events.emit(
        "order_review",
        serde_json::json!({"id": report.id, "reviewer": reviewer,
        "capsule_id": capsule_id, "review_nonce": binding.nonce,
        "candidate_snapshot_sha256": binding.candidate_snapshot_sha256,
        "diff_sha256": binding.diff_sha256, "verdict": summary.verdict,
        "findings": summary.findings.len(), "detail": summary.detail}),
    )?;
    report.review = Some(summary);
    Ok(decision)
}
