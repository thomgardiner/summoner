use super::GroveCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Release-qualified Grove contract Summoner was tested against.
const VERSION: &str = "0.4.0";
const TASK_STATUS_SCHEMA: u64 = 4;
const TASK_RECORD_SCHEMA: u64 = 6;

#[derive(Deserialize)]
struct Report {
    schema_version: u64,
    grove_version: String,
    status: Status,
    task: Task,
    inspection: Inspection,
}
#[derive(Deserialize)]
struct Task {
    exec_capabilities: Vec<String>,
    verification_policy_pinned: bool,
}
#[derive(Deserialize)]
struct Status {
    repository_schema: u64,
    task_status_schema: u64,
    task_record_schema: u64,
}
#[derive(Deserialize)]
struct Inspection {
    binding_schema: u64,
    execution_schema: u64,
    process_tree: String,
    filesystem: String,
    output: String,
    finish_source_cas: bool,
}

#[derive(Serialize)]
pub struct Capabilities {
    pub version: String,
    pub repository_schema: u64,
    pub task_status_schema: u64,
    pub task_record_schema: u64,
    pub exec_capabilities: Vec<String>,
    pub verification_policy_pinned: bool,
    pub inspection_binding_schema: u64,
    pub inspection_execution_schema: u64,
    pub process_tree: String,
    pub filesystem: String,
    pub output: String,
    pub finish_source_cas: bool,
}

pub(super) fn check(cli: &GroveCli) -> Result<Capabilities> {
    let value = cli
        .domain(Path::new("."), &["capabilities"])
        .context("Grove does not expose the required capabilities report")?;
    let report: Report = serde_json::from_value(value).context("parsing Grove capabilities")?;
    let mut miss = Vec::new();
    if report.schema_version != 1 {
        miss.push(format!("schema_version={} (need 1)", report.schema_version));
    }
    if !version_compatible(&report.grove_version, VERSION) {
        miss.push(format!(
            "grove_version={:?} (need compatible with {VERSION})",
            report.grove_version
        ));
    }
    if report.status.repository_schema != 1 {
        miss.push(format!(
            "repository_schema={} (need 1)",
            report.status.repository_schema
        ));
    }
    if report.status.task_status_schema != TASK_STATUS_SCHEMA {
        miss.push(format!(
            "task_status_schema={} (need {TASK_STATUS_SCHEMA})",
            report.status.task_status_schema
        ));
    }
    if report.status.task_record_schema != TASK_RECORD_SCHEMA {
        miss.push(format!(
            "task_record_schema={} (need {TASK_RECORD_SCHEMA})",
            report.status.task_record_schema
        ));
    }
    if !report
        .task
        .exec_capabilities
        .iter()
        .any(|name| name == "edit")
    {
        miss.push("exec_capabilities missing \"edit\"".into());
    }
    if !report.task.verification_policy_pinned {
        miss.push("verification_policy_pinned=false".into());
    }
    if report.inspection.binding_schema != 1 {
        miss.push(format!(
            "inspection.binding_schema={} (need 1)",
            report.inspection.binding_schema
        ));
    }
    if report.inspection.execution_schema != 1 {
        miss.push(format!(
            "inspection.execution_schema={} (need 1)",
            report.inspection.execution_schema
        ));
    }
    if report.inspection.filesystem != "read_only_permissions_and_digest" {
        miss.push(format!(
            "inspection.filesystem={:?}",
            report.inspection.filesystem
        ));
    }
    if report.inspection.output != "captured_logs_json_report" {
        miss.push(format!("inspection.output={:?}", report.inspection.output));
    }
    if !report.inspection.finish_source_cas {
        miss.push("inspection.finish_source_cas=false".into());
    }
    if !matches!(
        report.inspection.process_tree.as_str(),
        "windows_job_object" | "unix_process_group_best_effort"
    ) {
        miss.push(format!(
            "inspection.process_tree={:?}",
            report.inspection.process_tree
        ));
    }
    if !miss.is_empty() {
        bail!(
            "Grove on PATH is not compatible with Summoner's host pin ({VERSION}): {}. \
             Upgrade grove (need task_status_schema {TASK_STATUS_SCHEMA}), or set \
             [host] kind = \"git\" for independence without Grove.",
            miss.join("; ")
        );
    }
    Ok(Capabilities {
        version: report.grove_version,
        repository_schema: 1,
        task_status_schema: TASK_STATUS_SCHEMA,
        task_record_schema: TASK_RECORD_SCHEMA,
        exec_capabilities: report.task.exec_capabilities,
        verification_policy_pinned: true,
        inspection_binding_schema: 1,
        inspection_execution_schema: 1,
        process_tree: report.inspection.process_tree,
        filesystem: report.inspection.filesystem,
        output: report.inspection.output,
        finish_source_cas: true,
    })
}

/// Accept exact pin or any 0.4.x (and above in the same major once we ship 0.4).
fn version_compatible(got: &str, want: &str) -> bool {
    if got == want {
        return true;
    }
    let parse = |s: &str| -> Option<(u64, u64)> {
        let mut parts = s.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    };
    match (parse(got), parse(want)) {
        (Some((gm, gmin)), Some((wm, wmin))) => gm == wm && gmin >= wmin,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::version_compatible;

    #[test]
    fn accepts_same_and_newer_minor() {
        assert!(version_compatible("0.4.0", "0.4.0"));
        assert!(version_compatible("0.4.1", "0.4.0"));
        assert!(!version_compatible("0.3.5", "0.4.0"));
        assert!(!version_compatible("1.0.0", "0.4.0"));
    }
}
