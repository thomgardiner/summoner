//! Land result reporting and run selection.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::git::git;
use super::Plan;

#[allow(clippy::too_many_arguments)]
pub(crate) fn report_result(
    repo: &Path,
    run_dir: &Path,
    plan: &Plan,
    landed: &[Value],
    stopped: Option<Value>,
    aggregate: Option<Value>,
    integration_candidate: Option<Value>,
    dry_run: bool,
) -> Result<()> {
    let head = git(repo, &["rev-parse", "HEAD"]).unwrap_or_default();
    let planned: Vec<&str> = plan.order.iter().map(|c| c.id.as_str()).collect();
    let skipped: Vec<Value> = plan
        .skipped
        .iter()
        .map(|(id, reason)| json!({"id": id, "reason": reason}))
        .collect();
    let body = json!({
        "dry_run": dry_run,
        "repo": repo.display().to_string(),
        "head": head,
        "planned": planned,
        "landed": landed,
        "skipped": skipped,
        "stopped": stopped,
        "aggregate": aggregate,
        "integration_candidate": integration_candidate,
    });
    println!("{}", serde_json::to_string_pretty(&body)?);

    // Bind sealed I into the run's assurance envelope when land succeeds.
    if !dry_run
        && stopped.is_none()
        && let Some(integration) = integration_candidate.as_ref()
    {
        let path = run_dir.join("assurance_envelope.json");
        if path.exists() {
            let mut envelope: Value = serde_json::from_slice(
                &std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?,
            )
            .with_context(|| format!("parsing {}", path.display()))?;
            envelope["integration_candidate"] = integration.clone();
            // Re-hash without the self-digest field (same domain as assurance_envelope).
            if let Some(obj) = envelope.as_object_mut() {
                obj.remove("envelope_sha256");
            }
            let digest = {
                use sha2::{Digest, Sha256};
                use std::fmt::Write;
                let mut hash = Sha256::new();
                hash.update(b"summoner.assurance-envelope.v1\0");
                hash.update(serde_json::to_vec(&envelope).unwrap_or_default());
                let mut hex = String::with_capacity(64);
                for byte in hash.finalize() {
                    write!(&mut hex, "{byte:02x}").expect("hex");
                }
                hex
            };
            envelope["envelope_sha256"] = json!(digest);
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&envelope).context("serializing envelope")?,
            )
            .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

pub(crate) fn latest_finished_run(root: &Path) -> Result<PathBuf> {
    // Prefer mtime of report.json over lexicographic path names: run ids are
    // `{unix_secs}-{pid}`, and pid width makes string order unreliable.
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("no runs under {}", root.display()))?
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        let report = path.join("report.json");
        if !report.exists() {
            continue;
        }
        let modified = std::fs::metadata(&report)
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &best {
            Some((prev, _)) if modified <= *prev => {}
            _ => best = Some((modified, path)),
        }
    }
    best.map(|(_, path)| path).with_context(|| {
        format!(
            "no finished run with a report.json under {}",
            root.display()
        )
    })
}
