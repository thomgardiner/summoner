use crate::{candidate, config, executor, git, order, report};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run_fleet(
    repo: &Path,
    config: config::Config,
    orders: Vec<order::Order>,
    jobs: usize,
    stream: bool,
) -> Result<report::RunReport> {
    let root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let initial_head = git::resolve_commit(&root, "HEAD")?;
    validate_bases(&root, &orders)?;
    let run_id = format!(
        "{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        std::process::id()
    );
    let run_dir = std::env::temp_dir().join("summoner").join(&run_id);
    std::fs::create_dir_all(&run_dir).context("creating run log directory")?;
    let (tx, rx) = mpsc::channel();
    let mut pending: BTreeMap<String, order::Order> = orders
        .into_iter()
        .map(|order| (order.id.clone(), order))
        .collect();
    let mut finished: BTreeMap<String, report::OrderReport> = BTreeMap::new();
    let mut running = 0usize;

    while !pending.is_empty() || running > 0 {
        let skip: Vec<String> = pending
            .values()
            .filter_map(|order| {
                let parent = order.after.first()?;
                let result = finished.get(parent)?;
                (result.outcome != "verified" || result.candidate_commit.is_none())
                    .then_some(order.id.clone())
            })
            .collect();
        for id in skip {
            let order = pending.remove(&id).expect("pending order exists");
            let parent = order.after.first().expect("skip has parent");
            let detail = format!("parent {parent:?} did not return a verified candidate");
            let report = report::OrderReport::skipped(&order, detail);
            if stream {
                report::print_event(&report);
            }
            finished.insert(id, report);
        }

        let ready: Vec<String> = pending
            .values()
            .filter_map(|order| {
                let Some(parent) = order.after.first() else {
                    return Some(order.id.clone());
                };
                finished
                    .get(parent)
                    .and_then(|result| result.candidate_commit.as_ref())
                    .map(|_| order.id.clone())
            })
            .take(jobs.saturating_sub(running))
            .collect();
        for id in ready {
            let order = pending.remove(&id).expect("pending order exists");
            let base = match resolve_base(&root, &order, &finished, &initial_head) {
                Ok(base) => base,
                Err(error) => {
                    let report = report::error_report(&order, error);
                    if stream {
                        report::print_event(&report);
                    }
                    finished.insert(order.id.clone(), report);
                    continue;
                }
            };
            let repo = root.clone();
            let config = config.clone();
            let run_dir = run_dir.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let result = run_one(&repo, &config, &order, &base, &run_dir);
                let report = result.unwrap_or_else(|error| report::error_report(&order, error));
                let _ = tx.send((order.id.clone(), report));
            });
            running += 1;
        }

        if running == 0 {
            if pending.is_empty() {
                break;
            }
            bail!("dependency queue made no progress");
        }
        let (id, result) = rx.recv().context("waiting for order worker")?;
        if stream {
            report::print_event(&result);
        }
        finished.insert(id, result);
        running -= 1;
    }
    let result = report::RunReport::new(
        run_id,
        root.display().to_string(),
        jobs,
        finished.into_values().collect(),
    );
    Ok(result)
}

fn validate_bases(repo: &Path, orders: &[order::Order]) -> Result<()> {
    for order in orders {
        if let Some(reference) = order.base.as_deref() {
            git::resolve_commit(repo, reference)
                .with_context(|| format!("order {} has invalid base {reference:?}", order.id))?;
        }
    }
    Ok(())
}

fn resolve_base(
    repo: &Path,
    order: &order::Order,
    finished: &BTreeMap<String, report::OrderReport>,
    initial_head: &str,
) -> Result<String> {
    let reference = order
        .after
        .first()
        .and_then(|parent| finished.get(parent))
        .and_then(|result| result.candidate_commit.clone())
        .or(order.base.clone())
        .unwrap_or_else(|| initial_head.to_string());
    git::resolve_commit(repo, &reference)
}

fn run_one(
    repo: &Path,
    config: &config::Config,
    order: &order::Order,
    base: &str,
    run_dir: &Path,
) -> Result<report::OrderReport> {
    let branch = order.branch.clone().unwrap_or_else(|| {
        format!(
            "smn/{}-{}",
            order.id,
            run_dir.file_name().unwrap().to_string_lossy()
        )
    });
    let worktree = run_dir.join("worktrees").join(&order.id);
    let mut result = report::OrderReport {
        id: order.id.clone(),
        title: order.title.clone(),
        outcome: "error".into(),
        detail: None,
        after: order.after.clone(),
        branch: Some(branch.clone()),
        base_commit: Some(base.to_string()),
        candidate_commit: None,
        changed_paths: Vec::new(),
        worktree: Some(worktree.display().to_string()),
        executor: None,
        verify: Vec::new(),
    };
    if let Err(error) = git::add_worktree(repo, &worktree, &branch, base) {
        result.detail = Some(error.to_string());
        return Ok(result);
    }

    let Some((executor, candidate, profile_name)) = executor::stage(
        executor::Context {
            repo,
            config,
            order,
            base,
            branch: &branch,
            worktree: &worktree,
            run_dir,
        },
        &mut result,
    ) else {
        return Ok(result);
    };
    let timeout = executor.timeout;
    let log_dir = executor.log_dir;
    let prompt_file = executor.prompt_file;
    let verification = match crate::verify::candidate(
        crate::verify::VerifyContext {
            config,
            order,
            profile_name: &profile_name,
            repo,
            base,
            candidate: &candidate,
            branch: &branch,
            worktree: &worktree,
            prompt_file: &prompt_file,
            timeout,
            log_dir: &log_dir,
        },
        &mut result,
    ) {
        Ok(verification) => verification,
        Err(error) => {
            result.outcome = "error".into();
            result.detail = Some(format!("worktree kept: {error}"));
            return Ok(result);
        }
    };
    let verifier_interrupted = result.verify.iter().any(|evidence| evidence.interrupted);
    if verifier_interrupted {
        result.outcome = "interrupted".into();
        if result.detail.is_none() {
            result.detail = Some("verification interrupted".into());
        }
    } else if verification.passed {
        result.outcome = "verified".into();
        result.detail = Some(format!("verified candidate {candidate}"));
    } else if result.detail.is_none() {
        result.detail = Some("verification failed".into());
        result.outcome = "verification_failed".into();
    } else {
        result.outcome = "verification_failed".into();
    }
    if !verification.retain
        && !candidate::cleanup(repo, &worktree, &branch, &candidate, &mut result)
    {
        result.outcome = "error".into();
    }
    Ok(result)
}
