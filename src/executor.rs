use crate::{candidate, config, order, process, report};
use anyhow::{Context as AnyhowContext, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Context<'a> {
    pub repo: &'a Path,
    pub config: &'a config::Config,
    pub order: &'a order::Order,
    pub base: &'a str,
    pub branch: &'a str,
    pub worktree: &'a Path,
    pub run_dir: &'a Path,
}

pub struct ExecutorRun {
    pub evidence: report::CommandEvidence,
    pub log_dir: PathBuf,
    pub prompt_file: PathBuf,
    pub timeout: u64,
}

pub fn stage(
    ctx: Context<'_>,
    result: &mut report::OrderReport,
) -> Option<(ExecutorRun, String, String)> {
    let executor = match run(
        ctx.config,
        ctx.order,
        ctx.base,
        ctx.branch,
        ctx.worktree,
        ctx.run_dir,
    ) {
        Ok(executor) => executor,
        Err(error) => {
            result.outcome = "executor_failed".into();
            result.detail = Some(error.to_string());
            candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, ctx.base, result);
            return None;
        }
    };
    result.executor = Some(executor.evidence.clone());
    match candidate::capture(
        ctx.order,
        ctx.repo,
        ctx.worktree,
        ctx.branch,
        ctx.base,
        result,
    ) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => {
            result.outcome = "error".into();
            result.detail = Some(format!("worktree kept: {error}"));
            return None;
        }
    }
    let candidate = result
        .candidate_commit
        .as_deref()
        .unwrap_or(ctx.base)
        .to_string();
    if executor.evidence.interrupted {
        result.outcome = "interrupted".into();
        result.detail = Some("executor interrupted".into());
        candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, &candidate, result);
        return None;
    }
    if executor.evidence.timed_out {
        result.outcome = "timeout".into();
        result.detail = Some(format!("executor exceeded {}s timeout", executor.timeout));
        candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, &candidate, result);
        return None;
    }
    if executor.evidence.exit != Some(0) {
        result.outcome = "executor_failed".into();
        result.detail = Some(format!("executor exited {:?}", executor.evidence.exit));
        candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, &candidate, result);
        return None;
    }
    if result.candidate_commit.is_none() {
        result.outcome = "unverified".into();
        result.detail = Some("executor produced no changes".into());
        candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, ctx.base, result);
        return None;
    }
    let Some(profile_name) = ctx
        .order
        .verify_profile
        .as_deref()
        .or(ctx.config.default_verify_profile.as_deref())
    else {
        result.outcome = "unverified".into();
        result.detail = Some("no verification profile configured".into());
        candidate::cleanup(ctx.repo, ctx.worktree, ctx.branch, &candidate, result);
        return None;
    };
    Some((executor, candidate, profile_name.to_string()))
}

fn run(
    config: &config::Config,
    order: &order::Order,
    base: &str,
    branch: &str,
    worktree: &Path,
    run_dir: &Path,
) -> Result<ExecutorRun> {
    let name = order
        .executor
        .as_deref()
        .or(config.default_executor.as_deref())
        .context("executor disappeared after validation")?;
    let backend = config
        .executors
        .get(name)
        .context("executor disappeared after validation")?;
    if let Some(missing) = backend
        .env_required
        .iter()
        .find(|name| std::env::var_os(name).is_none())
    {
        bail!("required environment variable {missing} is missing");
    }
    let log_dir = run_dir.join("logs").join(&order.id);
    std::fs::create_dir_all(&log_dir)?;
    let prompt = compose_prompt(order, base, worktree, branch);
    let git_common_dir = crate::git::common_dir(worktree)?;
    let prompt_file = log_dir.join("prompt.txt");
    if matches!(backend.prompt, config::PromptRouting::File) {
        std::fs::write(&prompt_file, &prompt)?;
    }
    let mut argv = backend.argv.clone();
    let had_prompt = argv.iter().any(|arg| arg.contains("{prompt}"));
    let had_prompt_file = argv.iter().any(|arg| arg.contains("{prompt_file}"));
    for arg in &mut argv {
        *arg = expand(
            arg,
            base,
            worktree,
            branch,
            &prompt,
            &prompt_file,
            &git_common_dir,
        );
    }
    match backend.prompt {
        config::PromptRouting::Arg if !had_prompt => argv.push(prompt.clone()),
        config::PromptRouting::File if !had_prompt_file => {
            argv.push(prompt_file.display().to_string())
        }
        _ => {}
    }
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    command
        .env("SUMMONER_BASE", base)
        .env("SUMMONER_WORKTREE", worktree)
        .env("SUMMONER_BRANCH", branch)
        .env("SUMMONER_ORDER", &order.id)
        .env("SUMMONER_GIT_COMMON_DIR", &git_common_dir);
    let timeout = config::timeout(config, backend, order.timeout_secs);
    let evidence = process::run(
        command,
        argv,
        worktree,
        timeout,
        matches!(backend.prompt, config::PromptRouting::Stdin).then_some(prompt.as_bytes()),
        &log_dir.join("executor.stdout.log"),
        &log_dir.join("executor.stderr.log"),
    )?;
    Ok(ExecutorRun {
        evidence,
        log_dir,
        prompt_file,
        timeout,
    })
}

fn compose_prompt(order: &order::Order, base: &str, worktree: &Path, branch: &str) -> String {
    let acceptance = if order.acceptance.is_empty() {
        "(none listed)".into()
    } else {
        order
            .acceptance
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are executing order {id}: {title}\n\n{brief}\n\nScope: {scope}\nAcceptance:\n{acceptance}\n\nWorktree: {worktree}\nBranch: {branch}\nBase commit: {base}\nMake the requested changes only, then commit them on the current branch.",
        id = order.id,
        title = order.title,
        brief = order.brief,
        scope = order.scope.join(", "),
        acceptance = acceptance,
        worktree = worktree.display(),
        branch = branch,
        base = base,
    )
}

fn expand(
    value: &str,
    base: &str,
    worktree: &Path,
    branch: &str,
    prompt: &str,
    prompt_file: &Path,
    git_common_dir: &Path,
) -> String {
    value
        .replace("{base}", base)
        .replace("{worktree}", &worktree.display().to_string())
        .replace("{branch}", branch)
        .replace("{prompt}", prompt)
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{git_common_dir}", &git_common_dir.display().to_string())
}
