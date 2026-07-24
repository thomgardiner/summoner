//! Land result reporting and run selection.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use super::Plan;
use super::git::git;

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

    // Idempotent: successful path already bound I before FF; re-bind is a no-op
    // refresh after HEAD advanced (same integration fields).
    if !dry_run && stopped.is_none() {
        bind_integration_envelope(run_dir, integration_candidate.as_ref())?;
    }
    Ok(())
}

/// Write sealed I into `assurance_envelope.json` when the file exists.
/// Called before FF so land evidence is durable if advance crashes mid-way.
pub(crate) fn bind_integration_envelope(
    run_dir: &Path,
    integration_candidate: Option<&Value>,
) -> Result<()> {
    let Some(integration) = integration_candidate else {
        return Ok(());
    };
    let path = run_dir.join("assurance_envelope.json");
    if !path.exists() {
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn latest_finished_run_prefers_newer_report_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Lexically later path would win under path sort; mtime must win instead.
        let lexically_first = root.join("1000-1");
        let lexically_later = root.join("1000-99999");
        std::fs::create_dir_all(&lexically_first).unwrap();
        std::fs::create_dir_all(&lexically_later).unwrap();
        std::fs::write(lexically_later.join("report.json"), b"{}").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        // Touch lexically_first last so it has the newer mtime.
        std::fs::write(lexically_first.join("report.json"), b"{}").unwrap();
        let picked = latest_finished_run(root).unwrap();
        assert_eq!(picked, lexically_first);
    }

    #[test]
    fn bind_integration_envelope_sets_candidate_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assurance_envelope.json");
        std::fs::write(
            &path,
            r#"{"schema":"summoner.assurance-envelope.v1","envelope_sha256":"old"}"#,
        )
        .unwrap();
        let integration = json!({"integration_commit":"abc123"});
        bind_integration_envelope(dir.path(), Some(&integration)).unwrap();
        let body: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            body["integration_candidate"]["integration_commit"],
            "abc123"
        );
        assert_eq!(body["envelope_sha256"].as_str().unwrap().len(), 64);
        assert_ne!(body["envelope_sha256"], "old");
    }
}
