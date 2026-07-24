//! Trusted-policy checks for orders.

use crate::config::Config;
use crate::order::Order;

pub(crate) fn policy_problems(
    order: &Order,
    config: &Config,
    policy: &crate::config::TrustedPolicy,
) -> Vec<String> {
    let mut problems = Vec::new();
    let reviewer = order.reviewer_name(config);
    if policy.require_reviewer && reviewer.is_none() {
        problems.push("trusted policy requires an independent reviewer".to_string());
    }
    policy_revocation_problems(order, config, policy, reviewer.as_deref(), &mut problems);
    if let Some(required) = policy.required_host.as_deref()
        && let Some(configured) = config.host.as_ref().and_then(|h| h.kind.as_deref())
        && !configured.eq_ignore_ascii_case(required)
    {
        problems.push(format!(
            "trusted policy requires host {required:?}, but config selects {configured:?}"
        ));
    }
    policy_reviewer_executor_problems(order, config, policy, reviewer.as_deref(), &mut problems);
    if let Some(executor) = order.executor_name(config)
        && !policy.allowed_executors.is_empty()
        && !policy
            .allowed_executors
            .iter()
            .any(|name| name == &executor)
    {
        problems.push(format!(
            "trusted policy does not allow executor {executor:?} (allowed: {})",
            policy.allowed_executors.join(", ")
        ));
    }
    if !policy.allowed_profiles.is_empty() {
        let profile = order
            .verify_profile
            .clone()
            .or_else(|| config.default_verify_profile.clone());
        let allowed = profile
            .as_deref()
            .is_some_and(|name| policy.allowed_profiles.iter().any(|p| p == name));
        if !allowed {
            problems.push(format!(
                "trusted policy requires a verify_profile from [{}], got {}",
                policy.allowed_profiles.join(", "),
                profile.as_deref().unwrap_or("none")
            ));
        }
    }
    if !policy.required_profiles.is_empty() {
        let known = config
            .verification
            .as_ref()
            .map(|v| v.profiles.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        // Grove host profiles live in .grove.toml; when no local verification
        // table exists we still require the names to be declared so the policy
        // is not a silent no-op.
        for profile in &policy.required_profiles {
            if !known.is_empty() && !known.iter().any(|p| p == profile) {
                problems.push(format!(
                    "trusted policy requires profile {profile:?} but it is not defined in [verification]"
                ));
            }
        }
    }
    problems
}

pub(crate) fn policy_revocation_problems(
    order: &Order,
    config: &Config,
    policy: &crate::config::TrustedPolicy,
    reviewer: Option<&str>,
    problems: &mut Vec<String>,
) {
    if let Some(executor) = order.executor_name(config)
        && policy
            .revoked_executors
            .iter()
            .any(|name| name == &executor)
    {
        problems.push(format!(
            "trusted policy revokes executor {executor:?} under epoch {}",
            policy.policy_epoch
        ));
    }
    if let Some(reviewer) = reviewer
        && policy.revoked_reviewers.iter().any(|name| name == reviewer)
    {
        problems.push(format!(
            "trusted policy revokes reviewer {reviewer:?} under epoch {}",
            policy.policy_epoch
        ));
    }
}

pub(crate) fn policy_reviewer_executor_problems(
    order: &Order,
    config: &Config,
    policy: &crate::config::TrustedPolicy,
    reviewer: Option<&str>,
    problems: &mut Vec<String>,
) {
    let Some(reviewer) = reviewer else {
        return;
    };
    let Some(executor) = order.executor_name(config) else {
        return;
    };
    if policy.distinct_reviewer_name && reviewer == executor {
        problems.push(format!(
            "trusted policy requires a reviewer distinct from executor {executor:?}"
        ));
    }
    if policy.distinct_reviewer_identity {
        let e_id = config
            .executors
            .get(&executor)
            .and_then(|b| b.identity.as_deref());
        let r_id = config
            .executors
            .get(reviewer)
            .and_then(|b| b.identity.as_deref());
        match (e_id, r_id) {
            (Some(e), Some(r)) if e == r => problems.push(format!(
                "trusted policy requires distinct reviewer identity; executor and reviewer both declare {e:?}"
            )),
            (None, _) | (_, None) => problems.push(
                "trusted policy requires distinct_reviewer_identity; set identity on both executor and reviewer backends"
                    .into(),
            ),
            _ => {}
        }
    }
    if !policy.allowed_reviewers.is_empty()
        && !policy.allowed_reviewers.iter().any(|name| name == reviewer)
    {
        problems.push(format!(
            "trusted policy does not allow reviewer {reviewer:?} (allowed: {})",
            policy.allowed_reviewers.join(", ")
        ));
    }
}
