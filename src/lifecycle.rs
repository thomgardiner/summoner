use crate::config::Config;
use crate::{order, presets};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
pub(crate) struct Executor {
    name: String,
    binary: String,
    found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<Diagnostic>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    env_missing: Vec<String>,
    timeout_secs: Option<u64>,
    auth_acknowledged: bool,
    ok: bool,
}

#[derive(Serialize)]
struct Diagnostic {
    label: String,
    auth: &'static str,
    argv: Vec<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup_hint: Option<String>,
}

pub(crate) fn roles(orders: &[order::Order], config: &Config) -> BTreeSet<String> {
    let mut roles = BTreeSet::new();
    for item in orders {
        roles.extend(item.executor_name(config));
        roles.extend(item.reviewer_name(config));
    }
    roles
}

pub(crate) fn inspect(
    config: &Config,
    roles: &BTreeSet<String>,
    next: &mut Vec<String>,
    allow_unknown_auth: bool,
) -> Result<Vec<Executor>> {
    let mut reports = Vec::new();
    for name in roles {
        let Some(backend) = config.executors.get(name) else {
            next.push(format!(
                "executor {name:?} is not configured; install its preset or edit {}",
                global_path()
            ));
            continue;
        };
        let binary = backend.argv.first().cloned().unwrap_or_default();
        let found = !binary.is_empty() && presets::on_path(&binary);
        let missing = backend
            .env_required
            .iter()
            .filter(|variable| std::env::var(variable).is_err())
            .cloned()
            .collect::<Vec<_>>();
        let diagnostic = presets::for_executor(name, backend)?
            .as_ref()
            .and_then(|preset| found.then(|| diagnose(preset)));
        let permitted = allow_unknown_auth || config.unknown_auth_allowed(name);
        let acknowledged = permitted
            && diagnostic
                .as_ref()
                .is_some_and(|check| check.auth == "unknown");
        steps(
            name,
            &binary,
            found,
            &missing,
            diagnostic.as_ref(),
            acknowledged,
            next,
        );
        let ok = found
            && missing.is_empty()
            && diagnostic.as_ref().is_none_or(|check| {
                check.ok || (acknowledged && check.auth == "unknown" && check.error.is_none())
            });
        reports.push(Executor {
            name: name.clone(),
            binary,
            found,
            diagnostic,
            env_missing: missing,
            timeout_secs: backend.timeout_secs,
            auth_acknowledged: acknowledged,
            ok,
        });
    }
    Ok(reports)
}

pub(crate) fn runnable(executor: &Executor) -> bool {
    executor.ok
}

fn diagnose(preset: &presets::Preset) -> Diagnostic {
    diagnostic(preset, presets::health(&preset.health_argv))
}

fn diagnostic(preset: &presets::Preset, result: std::result::Result<(), String>) -> Diagnostic {
    let auth = match (preset.auth_checked, result.is_ok()) {
        (true, true) => "passed",
        (true, false) => "failed",
        (false, _) => "unknown",
    };
    let ok = auth == "passed";
    Diagnostic {
        label: preset.health_label.clone(),
        auth,
        argv: preset.health_argv.clone(),
        ok,
        error: result.err(),
        setup_hint: (!ok).then(|| preset.setup_hint.clone()),
    }
}

fn steps(
    name: &str,
    binary: &str,
    found: bool,
    missing: &[String],
    diagnostic: Option<&Diagnostic>,
    acknowledged: bool,
    next: &mut Vec<String>,
) {
    if !found {
        next.push(format!(
            "install {binary:?} for executor {name:?}, ensure it is on PATH, then rerun `summoner doctor`"
        ));
    }
    for variable in missing {
        next.push(format!(
            "executor {name:?} needs ${variable}; export it or complete the CLI auth flow"
        ));
    }
    if let Some(check) = diagnostic
        && !check.ok
        && (check.auth != "unknown" || !acknowledged)
        && let Some(hint) = &check.setup_hint
    {
        next.push(format!("executor {name:?}: {hint}"));
    }
    if diagnostic.is_some_and(|check| check.auth == "unknown" && check.error.is_none())
        && !acknowledged
    {
        next.push(format!(
            "executor {name:?} cannot prove authentication; rerun with --allow-unknown-auth or persist its name in allow_unknown_auth"
        ));
    }
}

fn global_path() -> String {
    crate::config::global_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the global Summoner config".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_kimi_config_check_requires_explicit_acknowledgement() {
        let preset = presets::get(presets::PresetName::Kimi).unwrap();
        let check = diagnostic(&preset, Ok(()));
        assert!(!check.ok);
        assert_eq!(check.auth, "unknown");
        assert_eq!(
            check.setup_hint.as_deref(),
            Some(preset.setup_hint.as_str())
        );
        let executor = Executor {
            name: "kimi".to_string(),
            binary: "kimi".to_string(),
            found: true,
            diagnostic: Some(check),
            env_missing: Vec::new(),
            timeout_secs: None,
            auth_acknowledged: false,
            ok: false,
        };
        assert!(!runnable(&executor));
    }
}
