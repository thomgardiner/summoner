use super::GroveCli;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

const VERSION: &str = "0.3.3";

#[derive(Deserialize)]
struct Report {
    schema_version: u64,
    grove_version: String,
    status: Status,
    inspection: Inspection,
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
}

#[derive(Serialize)]
pub struct Capabilities {
    pub version: String,
    pub repository_schema: u64,
    pub task_status_schema: u64,
    pub task_record_schema: u64,
    pub inspection_binding_schema: u64,
    pub inspection_execution_schema: u64,
    pub process_tree: String,
    pub filesystem: String,
    pub output: String,
}

pub(super) fn check(cli: &GroveCli) -> Result<Capabilities> {
    let value = cli
        .domain(Path::new("."), &["capabilities"])
        .context("Grove does not expose the required capabilities report")?;
    let report: Report = serde_json::from_value(value).context("parsing Grove capabilities")?;
    let exact = report.schema_version == 1
        && report.grove_version == VERSION
        && report.status.repository_schema == 1
        && report.status.task_status_schema == 2
        && report.status.task_record_schema == 4
        && report.inspection.binding_schema == 1
        && report.inspection.execution_schema == 1
        && report.inspection.filesystem == "read_only_permissions_and_digest"
        && report.inspection.output == "captured_logs_json_report"
        && matches!(
            report.inspection.process_tree.as_str(),
            "windows_job_object" | "unix_process_group_best_effort"
        );
    if !exact {
        bail!("Grove capabilities do not match the release-qualified 0.3.3 inspection contract")
    }
    Ok(Capabilities {
        version: report.grove_version,
        repository_schema: 1,
        task_status_schema: 2,
        task_record_schema: 4,
        inspection_binding_schema: 1,
        inspection_execution_schema: 1,
        process_tree: report.inspection.process_tree,
        filesystem: report.inspection.filesystem,
        output: report.inspection.output,
    })
}
