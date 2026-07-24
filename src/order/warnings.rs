//! Soft order warnings.

use crate::config::Config;
use crate::order::Order;
use crate::order::depends_on;
use std::collections::BTreeMap;

/// Identical scope strings across orders are not an error (grove serializes
/// them: the later `task begin` reports a conflict and the order lands as
/// blocked), but they are almost always an orchestrator mistake worth naming
/// before worktrees are spent on them. Variant siblings share a claim group,
/// so their deliberate overlap is not a mistake and stays quiet.
pub fn warnings(orders: &[Order], config: &Config) -> Vec<String> {
    let mut first_owner: BTreeMap<&str, &Order> = BTreeMap::new();
    let mut warnings = Vec::new();
    for order in orders {
        // A backend judging its own vendor's work loses the fresh-eyes
        // independence the gate exists for; different vendors catch more.
        if let (Some(reviewer), Some(executor)) =
            (order.reviewer_name(config), order.executor_name(config))
            && reviewer == executor
        {
            warnings.push(format!(
                "order {:?}: reviewer and executor are both {executor:?}; \
                 an independent review wants a different backend",
                order.id
            ));
        }
    }
    for order in orders {
        for entry in &order.scope {
            match first_owner.get(entry.as_str()) {
                Some(owner)
                    if owner.claim_group.is_none() || owner.claim_group != order.claim_group =>
                {
                    if !depends_on(orders, &order.id, &owner.id)
                        && !depends_on(orders, &owner.id, &order.id)
                    {
                        warnings.push(format!(
                            "orders {:?} and {:?} both claim scope {entry:?}; \
                             add an after edge so they do not block",
                            owner.id, order.id
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    first_owner.insert(entry, order);
                }
            }
        }
    }
    warnings
}
