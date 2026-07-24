use crate::config::Config;
use crate::host;
use crate::order::Order;
use crate::report::{OrderReport, Outcome};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Re-run an earlier fleet from its immutable manifest and authoritative
/// journal. The active host remains the source of truth for task lifecycle.
pub fn resume(
    current: &Config,
    run_id: &str,
    stream: bool,
    allow_unknown_auth: bool,
) -> Result<i32> {
    let run_dir = crate::run::runs_root().join(run_id);
    let replay = crate::run_manifest::replay(&run_dir, run_id, current)?;
    let config = replay.config;
    enforce_live_policy_floor(current, &config, run_id)?;
    refuse_live_revocations(current, &replay.orders)?;
    crate::config::selected_profile(replay.selected_profile.as_deref());
    let repo = std::env::current_dir().context("resolving current directory")?;
    let recorded_host = crate::run_manifest::recorded_host_kind(&run_dir)?;
    let host = host::open(&config, &repo)?;
    let live = host.preflight()?;
    if let Some(recorded) = recorded_host.as_deref()
        && recorded != live.kind
    {
        bail!(
            "run {run_id} was recorded under host {recorded:?} but config resolves to {:?}; set [host] kind = {recorded:?} to resume",
            live.kind
        );
    }
    crate::doctor::require(&config, &replay.orders, allow_unknown_auth)?;

    let records = crate::run_journal::records(&run_dir.join("events.jsonl"), run_id)?;
    let mut history = histories(records)?;
    let tasks = task_evidence(host.task_status(&repo)?)?;
    let known: BTreeSet<&str> = replay
        .orders
        .iter()
        .map(|order| order.id.as_str())
        .collect();
    if let Some(id) = history.keys().find(|id| !known.contains(id.as_str())) {
        bail!("run journal contains evidence for unknown order {id:?}");
    }

    let mut carried = Vec::new();
    let mut prior = Vec::new();
    let mut orders = replay.orders;
    for order in &mut orders {
        let mut state = history.remove(&order.id).unwrap_or_default();
        let task = state.task_id.as_ref().and_then(|id| tasks.get(id));
        if let Some(task) = task
            && !matches!(task.status.as_str(), "finished" | "abandoned")
        {
            bail!(
                "order {:?} still owns Grove task {} (status {}); retry after it finishes, or abandon it explicitly before resuming",
                order.id,
                task.id,
                task.status
            );
        }

        if state
            .report
            .as_ref()
            .is_some_and(|report| matches!(report.outcome, Outcome::Verified | Outcome::Approved))
        {
            let mut report = state.report.take().expect("checked above");
            let task = task.ok_or_else(|| {
                anyhow!(
                    "order {:?} records {:?} but Grove task {:?} is missing",
                    order.id,
                    report.outcome,
                    report.task_id
                )
            })?;
            agree(order, &config, &report, task)?;
            cleanup(host.as_ref(), task, state.worktree.as_deref(), &mut report)?;
            report.detail = Some(match report.detail.take() {
                Some(detail) => format!("{detail}; carried from run {run_id}"),
                None => format!("carried from run {run_id}"),
            });
            carried.push(report);
            continue;
        }

        if let Some(task) = task {
            if let Some(report) = state.report.as_mut() {
                cleanup(host.as_ref(), task, state.worktree.as_deref(), report)?;
            } else if state
                .worktree
                .as_ref()
                .is_some_and(|path| Path::new(path).exists())
            {
                let path = Path::new(state.worktree.as_deref().expect("checked above"));
                host.worktree_release(&repo, path)
                    .with_context(|| format!("releasing terminal worktree {}", path.display()))?;
            }
        } else if state
            .worktree
            .as_ref()
            .is_some_and(|path| Path::new(path).exists())
        {
            bail!(
                "order {:?} has worktree {} but host task {:?} is missing; recover that worktree before resuming",
                order.id,
                state.worktree.as_deref().unwrap_or_default(),
                state.task_id
            );
        }

        if order.branch.is_none() {
            order.branch.clone_from(&state.branch);
        }
        prior.push(state.into_report(order, &config));
    }
    crate::run::execute(&config, host, orders, stream, carried, prior)
}

#[derive(Default)]
struct History {
    task_id: Option<String>,
    worktree: Option<String>,
    branch: Option<String>,
    attempts: u64,
    session_id: Option<String>,
    usage_tokens: Option<u64>,
    executor_exit: Option<i32>,
    report: Option<OrderReport>,
    terminal: bool,
}

impl History {
    fn report(&mut self, report: OrderReport, terminal: bool) -> Result<()> {
        if terminal && self.terminal {
            bail!(
                "run journal has duplicate terminal records for {:?}",
                report.id
            );
        }
        self.task_id.clone_from(&report.task_id);
        self.worktree.clone_from(&report.worktree);
        self.branch.clone_from(&report.branch);
        self.attempts = report.attempts;
        self.session_id.clone_from(&report.session_id);
        self.usage_tokens = report.usage_tokens;
        self.executor_exit = report.executor_exit;
        self.report = Some(report);
        self.terminal |= terminal;
        Ok(())
    }

    fn into_report(self, order: &Order, config: &Config) -> OrderReport {
        if let Some(report) = self.report {
            return report;
        }
        let mut report = OrderReport::new(order, order.executor_name(config).unwrap_or_default());
        report.detail = Some("resuming incomplete durable history".into());
        report.task_id = self.task_id;
        report.worktree = self.worktree;
        report.branch = self.branch;
        report.attempts = self.attempts.max(1);
        report.session_id = self.session_id;
        report.usage_tokens = self.usage_tokens;
        report.executor_exit = self.executor_exit;
        report
    }
}

fn histories(records: Vec<serde_json::Value>) -> Result<BTreeMap<String, History>> {
    let mut histories = BTreeMap::<String, History>::new();
    for record in records {
        let event = record.get("event").and_then(serde_json::Value::as_str);
        let Some(id) = record.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let state = histories.entry(id.to_string()).or_default();
        match event {
            Some("order_dispatched") => {
                state.task_id = text(&record, "task_id");
                state.worktree = text(&record, "worktree");
                state.branch = text(&record, "branch");
            }
            Some("order_revised") => state.task_id = text(&record, "task_id"),
            Some("order_exec_done") => {
                state.attempts = record["attempt"].as_u64().unwrap_or(state.attempts);
                state.session_id = text(&record, "session_id").or(state.session_id.take());
                state.usage_tokens = record["usage_tokens"].as_u64().or(state.usage_tokens);
                state.executor_exit = record["exit"].as_i64().map(|exit| exit as i32);
            }
            Some("order_checkpoint" | "order_finished" | "order_carried") => {
                let report: OrderReport = serde_json::from_value(
                    record
                        .get("report")
                        .cloned()
                        .ok_or_else(|| anyhow!("{event:?} for {id:?} has no report"))?,
                )
                .with_context(|| format!("reading {event:?} report for {id:?}"))?;
                if report.id != id {
                    bail!(
                        "{event:?} id {id:?} disagrees with report id {:?}",
                        report.id
                    );
                }
                state.report(report, !matches!(event, Some("order_checkpoint")))?;
            }
            _ => {}
        }
    }
    Ok(histories)
}

fn text(record: &serde_json::Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(String::from)
}

/// Live policy is the safety floor; recorded policy authorized the original run.
fn enforce_live_policy_floor(current: &Config, recorded: &Config, run_id: &str) -> Result<()> {
    let Some(live_policy) = current.trusted_policy.as_ref() else {
        return Ok(());
    };
    live_policy
        .verify_signature()
        .context("live trusted_policy signature check failed on resume")?;
    let recorded_epoch = recorded
        .trusted_policy
        .as_ref()
        .map(|policy| policy.policy_epoch)
        .unwrap_or(0);
    if !live_policy.allows_resume_of(recorded_epoch) {
        bail!(
            "run {run_id} was recorded under trusted_policy epoch {recorded_epoch}, but the live policy requires minimum_resumable_epoch {}; re-run under the current policy instead of resume",
            live_policy.minimum_resumable_epoch
        );
    }
    Ok(())
}

/// Emergency bans on the live policy apply to residual resume work even when the
/// recorded policy authorized the original dispatch.
fn refuse_live_revocations(current: &Config, orders: &[Order]) -> Result<()> {
    let Some(live) = current.trusted_policy.as_ref() else {
        return Ok(());
    };
    if live.revoked_executors.is_empty() && live.revoked_reviewers.is_empty() {
        return Ok(());
    }
    let mut problems = Vec::new();
    for order in orders {
        let executor = order.executor_name(current);
        if let Some(executor) = executor.as_deref()
            && live.revoked_executors.iter().any(|name| name == executor)
        {
            problems.push(format!(
                "order {:?}: live trusted policy revokes executor {executor:?}",
                order.id
            ));
        }
        if let Some(reviewer) = order.reviewer_name(current)
            && live.revoked_reviewers.iter().any(|name| name == &reviewer)
        {
            problems.push(format!(
                "order {:?}: live trusted policy revokes reviewer {reviewer:?}",
                order.id
            ));
        }
    }
    if !problems.is_empty() {
        bail!(
            "live trusted policy forbids resuming these orders: {}",
            problems.join("; ")
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct TaskBoard {
    schema_version: u32,
    tasks: Vec<TaskEvidence>,
}

#[derive(Deserialize)]
struct TaskEvidence {
    id: String,
    status: String,
    recorded_verification: String,
    source_sha256: Option<String>,
}

fn task_evidence(value: serde_json::Value) -> Result<BTreeMap<String, TaskEvidence>> {
    let board: TaskBoard = serde_json::from_value(value).context("parsing Grove task status")?;
    if board.schema_version != 4 {
        bail!(
            "Grove task status schema {} cannot reconcile durable approval; need schema 4",
            board.schema_version
        );
    }
    let mut tasks = BTreeMap::new();
    for task in board.tasks {
        if tasks.insert(task.id.clone(), task).is_some() {
            bail!("Grove task status contains a duplicate task id");
        }
    }
    Ok(tasks)
}

fn agree(order: &Order, config: &Config, report: &OrderReport, task: &TaskEvidence) -> Result<()> {
    if task.status != "finished" {
        bail!(
            "order {:?} is green but Grove task {} is {}",
            order.id,
            task.id,
            task.status
        );
    }
    if report.task_id.as_deref() != Some(task.id.as_str()) {
        bail!(
            "order {:?} green report points at a different Grove task",
            order.id
        );
    }
    let finish = report.finish.as_ref().ok_or_else(|| {
        anyhow!(
            "order {:?} is green but records no Grove finish evidence",
            order.id
        )
    })?;
    if !finish.verified || task.recorded_verification != "passed" {
        bail!(
            "order {:?} green report disagrees with Grove verification: journal verified={}, Grove recorded_verification={:?}",
            order.id,
            finish.verified,
            task.recorded_verification,
        );
    }
    match report.outcome {
        Outcome::Approved => {
            let expected = order.reviewer_name(config).ok_or_else(|| {
                anyhow!(
                    "order {:?} is approved but configured no reviewer",
                    order.id
                )
            })?;
            let review = report
                .review
                .as_ref()
                .ok_or_else(|| anyhow!("order {:?} is approved but records no review", order.id))?;
            if review.verdict != "approve" || review.reviewer != expected {
                bail!(
                    "order {:?} approval disagrees with its recorded reviewer",
                    order.id
                );
            }
            if task.source_sha256.as_deref() != Some(review.candidate_snapshot_sha256.as_str()) {
                bail!(
                    "order {:?} approval snapshot {} disagrees with Grove task source {:?}",
                    order.id,
                    review.candidate_snapshot_sha256,
                    task.source_sha256,
                );
            }
        }
        Outcome::Verified if order.reviewer_name(config).is_some() => {
            bail!("order {:?} bypassed its configured reviewer", order.id)
        }
        Outcome::Verified => {}
        _ => unreachable!("caller selects green outcomes"),
    }
    Ok(())
}

fn cleanup(
    host: &dyn host::Host,
    task: &TaskEvidence,
    worktree: Option<&str>,
    report: &mut OrderReport,
) -> Result<()> {
    let Some(path) = worktree.map(PathBuf::from).filter(|path| path.exists()) else {
        return Ok(());
    };
    let outcome = host
        .worktree_release(&std::env::current_dir()?, &path)
        .with_context(|| {
            format!(
                "releasing terminal host task {} at {}",
                task.id,
                path.display()
            )
        })?;
    report.saved_to = outcome.saved_to;
    if report.branch.is_none() {
        report.branch = outcome.branch;
    }
    report.release_error = None;
    Ok(())
}
