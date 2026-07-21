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
    Ok(orders)
}
