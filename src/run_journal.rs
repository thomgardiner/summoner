//! Validated reads of the authoritative run journal.

use crate::report::OrderReport;
use anyhow::{Context, Result, anyhow, bail};
use std::path::Path;

const SCHEMA_VERSION: u32 = 1;

/// Read every complete record. One invalid unterminated tail is a tolerated
/// crash append; corruption in any newline-terminated record is fatal.
pub(crate) fn records(path: &Path, run_id: &str) -> Result<Vec<serde_json::Value>> {
    let data =
        std::fs::read(path).with_context(|| format!("reading run journal {}", path.display()))?;
    let mut segments: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    let trailing = segments.pop().unwrap_or_default();
    let mut records = Vec::new();
    for (seq, line) in segments.into_iter().enumerate() {
        records.push(record(line, run_id, seq as u64)?);
    }
    if !trailing.is_empty()
        && let Ok(record) = record(trailing, run_id, records.len() as u64)
    {
        records.push(record);
    }
    Ok(records)
}

pub(crate) fn terminal_reports(path: &Path, run_id: &str) -> Result<Vec<OrderReport>> {
    records(path, run_id)?
        .into_iter()
        .filter(|record| {
            matches!(
                record.get("event").and_then(serde_json::Value::as_str),
                Some("order_finished" | "order_carried")
            )
        })
        .enumerate()
        .map(|(index, record)| {
            let payload = record
                .get("report")
                .cloned()
                .ok_or_else(|| anyhow!("terminal record {index} has no report payload"))?;
            serde_json::from_value(payload)
                .with_context(|| format!("projecting terminal order report {index}"))
        })
        .collect()
}

fn record(line: &[u8], run_id: &str, expected_seq: u64) -> Result<serde_json::Value> {
    let record: serde_json::Value = serde_json::from_slice(line)
        .with_context(|| format!("run journal has invalid JSON at seq {expected_seq}"))?;
    let schema = record
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if schema != Some(SCHEMA_VERSION as u64) {
        bail!("run journal schema mismatch at seq {expected_seq}: found {schema:?}");
    }
    match record.get("run_id").and_then(serde_json::Value::as_str) {
        Some(found) if found == run_id => {}
        found => bail!("run journal run-id mismatch at seq {expected_seq}: found {found:?}"),
    }
    if record.get("seq").and_then(serde_json::Value::as_u64) != Some(expected_seq) {
        bail!("run journal sequence gap at seq {expected_seq}");
    }
    Ok(record)
}
