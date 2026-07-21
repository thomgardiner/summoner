use crate::config::Config;
use crate::order::{self, Order};
use anyhow::{Result, bail};
use std::path::PathBuf;

/// Load, warn, and fail-fast validate a batch before anything is dispatched.
pub(crate) fn validated(paths: &[PathBuf], config: &Config) -> Result<Vec<Order>> {
    checked(order::load(paths)?, config)
}

pub(crate) fn checked(orders: Vec<Order>, config: &Config) -> Result<Vec<Order>> {
    for warning in order::warnings(&orders, config) {
        eprintln!("summoner: warning: {warning}");
    }
    let problems = order::validate(&orders, config);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("summoner: {problem}");
        }
        bail!("{} order problem(s); nothing dispatched", problems.len());
    }
    preflight_env(&orders, config)?;
    Ok(orders)
}

/// Missing executor environment fails in seconds with the fix named, not after
/// a full timeout inside the first order.
fn preflight_env(orders: &[Order], config: &Config) -> Result<()> {
    let mut missing = Vec::new();
    let mut checked = std::collections::BTreeSet::new();
    for order in orders {
        let names = [order.executor_name(config), order.reviewer_name(config)];
        for name in names.into_iter().flatten() {
            if !checked.insert(name.clone()) {
                continue;
            }
            if let Some(backend) = config.executors.get(&name) {
                for var in &backend.env_required {
                    if std::env::var(var).is_err() {
                        missing.push(format!(
                            "executor {name:?} needs ${var} (interactive-shell exports do not \
                             reach summoner; export it here or persist it via the backend's \
                             auth flow)"
                        ));
                    }
                }
            }
        }
    }
    if !missing.is_empty() {
        bail!("{}", missing.join("\n"));
    }
    Ok(())
}
