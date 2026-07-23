use super::GroveCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

const VERSION: &str = "0.4.0";

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
    let exact = report.schema_version == 1
        && report.grove_version == VERSION
        && report.status.repository_schema == 1
        && report.status.task_status_schema == 4
        && report.status.task_record_schema == 6
        // Executors run under `task exec --capability edit`; a Grove without it
        // would either reject the flag or silently hold a lane per session.
        && report.task.exec_capabilities.iter().any(|name| name == "edit")
        && report.task.verification_policy_pinned
        && report.inspection.binding_schema == 1
        && report.inspection.execution_schema == 1
        && report.inspection.filesystem == "read_only_permissions_and_digest"
        && report.inspection.output == "captured_logs_json_report"
        && report.inspection.finish_source_cas
        && matches!(
            report.inspection.process_tree.as_str(),
            "windows_job_object" | "unix_process_group_best_effort"
        );
    if !exact {
        bail!(
            "Grove capabilities do not match the release-qualified {VERSION} task and \
             inspection contract"
        )
    }
    Ok(Capabilities {
        version: report.grove_version,
        repository_schema: 1,
        task_status_schema: 4,
        task_record_schema: 6,
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
