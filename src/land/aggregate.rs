//! Aggregate verification before fast-forward.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

pub(crate) fn aggregate_verify(repo: &Path) -> Result<Value> {
    if let Ok(raw) = std::env::var("SUMMONER_LAND_VERIFY") {
        let argv: Vec<&str> = if raw.contains('\u{1f}') {
            raw.split('\u{1f}').filter(|s| !s.is_empty()).collect()
        } else {
            raw.split_whitespace().collect()
        };
        if argv.is_empty() {
            bail!("SUMMONER_LAND_VERIFY is empty");
        }
        let output = Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(repo)
            .output()
            .with_context(|| format!("running land verify {}", argv[0]))?;
        if !output.status.success() {
            bail!(
                "{} exited {:?}: {}",
                argv[0],
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(json!({
            "command": argv,
            "passed": true,
        }));
    }
    if repo.join("Cargo.toml").is_file() {
        let output = Command::new("cargo")
            .args(["test", "--locked", "--", "--test-threads=1"])
            .current_dir(repo)
            .output()
            .context("running cargo test as land aggregate verify")?;
        if !output.status.success() {
            bail!(
                "cargo test exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        return Ok(json!({
            "command": ["cargo", "test", "--locked"],
            "passed": true,
        }));
    }
    if std::env::var_os("SUMMONER_LAND_ALLOW_NO_AGGREGATE").is_some() {
        return Ok(json!({
            "command": [],
            "passed": true,
            "detail": "SUMMONER_LAND_ALLOW_NO_AGGREGATE set; aggregate gate skipped",
        }));
    }
    bail!(
        "land refuses to advance the protected branch without an aggregate verify: set SUMMONER_LAND_VERIFY to an argv, add a root Cargo.toml (cargo test), or set SUMMONER_LAND_ALLOW_NO_AGGREGATE=1 for an explicit no-gate landing"
    )
}
