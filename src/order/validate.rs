//! Order validation orchestration.

use crate::config::Config;
use crate::order::Order;
use crate::order::backend::backend_problems;
use crate::order::git_host_active;
use crate::order::graph::cycle_members;
use crate::order::policy::policy_problems;
use std::collections::BTreeSet;

pub fn validate(orders: &[Order], config: &Config) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut used_backends = BTreeSet::new();

    for order in orders {
        problems.extend(validate_order_fields(
            order,
            config,
            &mut seen_ids,
            &mut used_backends,
        ));
    }

    let expanded_ids: BTreeSet<&str> = orders
        .iter()
        .filter_map(|o| o.variant_of.as_deref())
        .collect();
    for order in orders {
        let at = order.source.display();
        for dep in &order.after {
            if dep == &order.id {
                problems.push(format!("{at}: after references the order itself"));
            } else if expanded_ids.contains(dep.as_str()) {
                problems.push(format!(
                    "{at}: after references {dep:?}, which expanded into variants; \
                     name a specific sibling such as \"{dep}-<executor>\""
                ));
            } else if !seen_ids.contains(dep) {
                problems.push(format!("{at}: after references unknown order {dep:?}"));
            }
        }
    }
    let cycle = cycle_members(orders);
    if !cycle.is_empty() {
        problems.push(format!(
            "dependency cycle among orders: {}",
            cycle.join(", ")
        ));
    }

    for name in used_backends {
        problems.extend(backend_problems(&name, &config.executors[&name]));
    }
    problems
}

fn validate_order_fields(
    order: &Order,
    config: &Config,
    seen_ids: &mut BTreeSet<String>,
    used_backends: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let at = order.source.display();
    let id_ok = !order.id.is_empty()
        && order
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !id_ok {
        problems.push(format!(
            "{at}: id {:?} must be non-empty [a-z0-9_-]+",
            order.id
        ));
    }
    if !seen_ids.insert(order.id.clone()) {
        problems.push(format!("{at}: duplicate id {:?}", order.id));
    }
    if order.title.trim().is_empty() {
        problems.push(format!("{at}: title is empty"));
    }
    if order.brief.trim().is_empty() {
        problems.push(format!("{at}: brief is empty"));
    }
    if order.scope.is_empty() || order.scope.iter().any(|s| s.trim().is_empty()) {
        problems.push(format!(
            "{at}: scope must be a non-empty list of non-empty entries"
        ));
    }
    // crate: claims need Cargo topology (grove host). On a resolved git
    // host they are opaque and unsafe to treat as path claims.
    if git_host_active(config) {
        for entry in &order.scope {
            if entry.starts_with("crate:") {
                problems.push(format!(
                    "{at}: scope entry {entry:?} uses crate: which requires the grove host; set [host] kind = \"grove\" or use filesystem paths"
                ));
            }
        }
    }
    if let Some(timeout) = order.timeout_secs
        && !(1..=604_800).contains(&timeout)
    {
        problems.push(format!(
            "{at}: timeout_secs must be between 1 and 604800 (7 days), got {timeout}"
        ));
    }
    // Expansion replaced variants orders with siblings; anything still
    // carrying both fields set them together, which is ambiguous.
    if !order.variants.is_empty() {
        problems.push(format!(
            "{at}: variants and executor are mutually exclusive (variants pick executors)"
        ));
    }
    match order.executor_name(config) {
        None => problems.push(format!(
            "{at}: no executor named and no default_executor configured"
        )),
        Some(name) => match config.executors.get(&name) {
            Some(backend) => {
                if order.max_tokens.is_some() && backend.usage_marker.is_none() {
                    problems.push(format!(
                        "{at}: max_tokens needs executor {name:?} to define a \
                         usage_marker, or the cap can never be measured"
                    ));
                }
                used_backends.insert(name);
            }
            None => problems.push(format!("{at}: executor {name:?} is not configured")),
        },
    }
    if let Some(name) = order.reviewer_name(config) {
        if config.executors.contains_key(&name) {
            used_backends.insert(name);
        } else {
            problems.push(format!("{at}: reviewer {name:?} is not configured"));
        }
    }
    if let Some(policy) = config.trusted_policy.as_ref() {
        problems.extend(
            policy_problems(order, config, policy)
                .into_iter()
                .map(|problem| format!("{at}: {problem}")),
        );
    }
    problems
}

#[cfg(test)]
#[path = "validate_tests.rs"]
mod tests;
