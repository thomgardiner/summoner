use crate::{config, git, order, process, report};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct VerifyContext<'a> {
    pub config: &'a config::Config,
    pub order: &'a order::Order,
    pub profile_name: &'a str,
    pub repo: &'a Path,
    pub base: &'a str,
    pub candidate: &'a str,
    pub branch: &'a str,
    pub worktree: &'a Path,
    pub prompt_file: &'a Path,
    pub timeout: u64,
    pub log_dir: &'a Path,
}

pub struct VerificationResult {
    pub passed: bool,
    pub retain: bool,
}

pub fn candidate(
    ctx: VerifyContext<'_>,
    result: &mut report::OrderReport,
) -> Result<VerificationResult> {
    let profile = ctx
        .config
        .verification
        .profiles
        .get(ctx.profile_name)
        .context("verification profile disappeared after validation")?;
    if let Err(error) = git::assert_candidate(ctx.repo, ctx.worktree, ctx.branch, ctx.candidate) {
        result.detail = Some(format!(
            "verification precondition failed; worktree kept: {error}"
        ));
        return Ok(VerificationResult {
            passed: false,
            retain: true,
        });
    }
    let verify_dir = ctx.log_dir.join("verify");
    std::fs::create_dir_all(&verify_dir)?;
    let mut all_passed = true;
    for (index, spec) in profile.commands.iter().enumerate() {
        let mut argv = spec.argv().to_vec();
        for arg in &mut argv {
            *arg = expand(arg, ctx.base, ctx.worktree, ctx.branch, ctx.prompt_file);
        }
        if argv.is_empty() {
            all_passed = false;
            result.detail = Some(format!("verification command {index} is empty"));
            break;
        }
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .env("SUMMONER_BASE", ctx.base)
            .env("SUMMONER_WORKTREE", ctx.worktree)
            .env("SUMMONER_BRANCH", ctx.branch)
            .env("SUMMONER_ORDER", &ctx.order.id);
        match process::run(
            command,
            argv,
            ctx.worktree,
            ctx.timeout,
            None,
            &verify_dir.join(format!("{index}.stdout.log")),
            &verify_dir.join(format!("{index}.stderr.log")),
        ) {
            Ok(evidence) => {
                all_passed &= evidence.success;
                result.verify.push(evidence);
            }
            Err(error) => {
                all_passed = false;
                result.detail = Some(format!("verification command {index}: {error}"));
            }
        }
        if let Err(error) = git::assert_candidate(ctx.repo, ctx.worktree, ctx.branch, ctx.candidate)
        {
            result.detail = Some(format!(
                "verification command {index} changed candidate; worktree kept: {error}"
            ));
            return Ok(VerificationResult {
                passed: false,
                retain: true,
            });
        }
        if !all_passed {
            break;
        }
    }
    Ok(VerificationResult {
        passed: all_passed,
        retain: false,
    })
}

fn expand(value: &str, base: &str, worktree: &Path, branch: &str, prompt_file: &Path) -> String {
    value
        .replace("{base}", base)
        .replace("{worktree}", &worktree.display().to_string())
        .replace("{branch}", branch)
        .replace("{prompt_file}", &prompt_file.display().to_string())
}
