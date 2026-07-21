use anyhow::{Context, Result, bail};
use getrandom::fill;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_FINDINGS: usize = 50;
const MAX_TEXT: usize = 2048;

#[derive(Clone)]
pub struct Binding {
    pub nonce: String,
    pub candidate_snapshot_sha256: String,
    pub diff_sha256: String,
    pub reviewer: String,
}

impl Binding {
    pub fn new(
        candidate_snapshot_sha256: String,
        diff_sha256: String,
        reviewer: &str,
    ) -> Result<Self> {
        let mut nonce = [0_u8; 32];
        fill(&mut nonce).map_err(|error| anyhow::anyhow!("generating review nonce: {error}"))?;
        Ok(Self {
            nonce: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
            candidate_snapshot_sha256,
            diff_sha256,
            reviewer: reviewer.to_string(),
        })
    }

    pub fn instructions(&self) -> String {
        serde_json::json!({
            "protocol_version": VERSION, "review_nonce": self.nonce,
            "candidate_snapshot_sha256": self.candidate_snapshot_sha256,
            "diff_sha256": self.diff_sha256, "verdict": "approve|reject",
            "findings": [{"severity":"blocker|major|minor","file":"path","line":1,"summary":"finding"}],
            "reviewer": {"provider": self.reviewer, "model":"report the actual model"}
        }).to_string() + "\nReturn exactly one JSON object with these fields and exact bindings; no prose or fencing."
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    protocol_version: u32,
    review_nonce: String,
    candidate_snapshot_sha256: String,
    diff_sha256: String,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    reviewer: Reviewer,
}

#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approve,
    Reject,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Severity,
    pub file: String,
    pub line: u64,
    pub summary: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reviewer {
    provider: String,
    model: String,
}

pub fn parse(output: &[u8], expected: &Binding) -> Result<Envelope> {
    if output.len() > MAX_OUTPUT {
        bail!("review verdict exceeds {MAX_OUTPUT} bytes")
    }
    let envelope: Envelope =
        serde_json::from_slice(output).context("review stdout is not one strict JSON object")?;
    if envelope.protocol_version != VERSION {
        bail!("unsupported review protocol version")
    }
    if envelope.review_nonce != expected.nonce
        || envelope.candidate_snapshot_sha256 != expected.candidate_snapshot_sha256
        || envelope.diff_sha256 != expected.diff_sha256
        || envelope.reviewer.provider != expected.reviewer
    {
        bail!("review verdict binding mismatch")
    }
    if envelope.findings.len() > MAX_FINDINGS {
        bail!("review verdict has too many findings")
    }
    if envelope.reviewer.model.is_empty() || envelope.reviewer.model.len() > MAX_TEXT {
        bail!("reviewer model is missing or oversized")
    }
    for finding in &envelope.findings {
        if finding.file.is_empty()
            || finding.file.len() > MAX_TEXT
            || finding.summary.is_empty()
            || finding.summary.len() > MAX_TEXT
        {
            bail!("review finding text is missing or oversized")
        }
    }
    if matches!(envelope.verdict, Verdict::Approve)
        && envelope
            .findings
            .iter()
            .any(|item| matches!(item.severity, Severity::Blocker))
    {
        bail!("approval contains a blocker finding")
    }
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            nonce: "n".into(),
            candidate_snapshot_sha256: "s".into(),
            diff_sha256: "d".into(),
            reviewer: "judge".into(),
        }
    }
    fn valid() -> serde_json::Value {
        serde_json::json!({"protocol_version":1,"review_nonce":"n","candidate_snapshot_sha256":"s","diff_sha256":"d","verdict":"approve","findings":[],"reviewer":{"provider":"judge","model":"fake"}})
    }
    #[test]
    fn exact_binding_parses() {
        assert!(parse(valid().to_string().as_bytes(), &binding()).is_ok());
    }
    #[test]
    fn injection_unknown_and_mismatched_bindings_fail_closed() {
        assert!(parse(format!("noise\n{}", valid()).as_bytes(), &binding()).is_err());
        for field in ["candidate_source_sha256", "candidate_tree", "extra"] {
            let mut unknown = valid();
            unknown[field] = serde_json::json!("unused");
            assert!(
                parse(unknown.to_string().as_bytes(), &binding()).is_err(),
                "{field}"
            );
        }
        for field in ["review_nonce", "candidate_snapshot_sha256", "diff_sha256"] {
            let mut wrong = valid();
            wrong[field] = serde_json::json!("wrong");
            assert!(
                parse(wrong.to_string().as_bytes(), &binding()).is_err(),
                "{field}"
            );
        }
    }
}
