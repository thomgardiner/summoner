//! Dependency graph helpers.

use crate::order::Order;
use std::collections::{BTreeMap, BTreeSet};



/// Kahn's elimination: whatever cannot be topologically drained is in (or
/// downstream of) a cycle. Unknown ids are reported separately and ignored here.
pub(crate) fn cycle_members(orders: &[Order]) -> Vec<String> {
    let ids: BTreeSet<&str> = orders.iter().map(|o| o.id.as_str()).collect();
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = orders
        .iter()
        .map(|order| {
            let wanted: BTreeSet<&str> = order
                .after
                .iter()
                .map(String::as_str)
                .filter(|dep| ids.contains(dep) && *dep != order.id)
                .collect();
            (order.id.as_str(), wanted)
        })
        .collect();
    loop {
        let ready: Vec<&str> = deps
            .iter()
            .filter(|(_, wanted)| wanted.is_empty())
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            break;
        }
        for id in ready {
            deps.remove(id);
            for wanted in deps.values_mut() {
                wanted.remove(id);
            }
        }
    }
    deps.keys().map(|id| id.to_string()).collect()
}

/// Whether `downstream` is transitively ordered after `upstream`.
pub(crate) fn depends_on(orders: &[Order], downstream: &str, upstream: &str) -> bool {
    let by_id: BTreeMap<&str, &Order> = orders
        .iter()
        .map(|order| (order.id.as_str(), order))
        .collect();
    let mut pending = vec![downstream];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        let Some(order) = by_id.get(id) else {
            continue;
        };
        for dependency in &order.after {
            if dependency == upstream {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                pending.push(dependency);
            }
        }
    }
    false
}
