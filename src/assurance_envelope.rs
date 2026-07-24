//! Compose a portable assurance envelope from a finished run report.
//!
//! The envelope is content-addressable evidence summary: policy identity,
//! per-order candidate identity, verification digests, and integration seal
//! when present. It does not re-run gates; it binds what already ran.

use crate::report::RunReport;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fmt::Write;

#[derive(Serialize)]
pub struct Envelope {
    pub schema_version: u32,
    pub envelope_sha256: String,
    pub run_id: String,
    pub repo: String,
    pub trusted_policy_sha256: Option<String>,
    pub trusted_policy_identity: Option<crate::config::PolicyIdentity>,
    pub orders: Vec<OrderBinding>,
    pub integration_candidate: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct OrderBinding {
    pub id: String,
    pub outcome: String,
    pub candidate_commit: Option<String>,
    pub candidate_id: Option<String>,
    pub source_sha256: Option<String>,
    pub verify_profiles: Vec<String>,
    pub review_binding: Option<String>,
}

/// Build an envelope from a run report plus optional land result JSON.
pub fn from_run(report: &RunReport, integration: Option<serde_json::Value>) -> Envelope {
    let orders: Vec<OrderBinding> = report
        .orders
        .iter()
        .map(|order| {
            let candidate_id = order
                .candidate_identity
                .as_ref()
                .and_then(|v| v["candidate_id"].as_str())
                .map(String::from);
            let source_sha256 = order
                .candidate_identity
                .as_ref()
                .and_then(|v| v["source_sha256"].as_str())
                .map(String::from);
            let verify_profiles = order.verify.iter().map(|v| v.profile.clone()).collect();
            let review_binding = order.review.as_ref().map(|r| {
                format!(
                    "{}:{}:{}",
                    r.verdict, r.candidate_snapshot_sha256, r.review_nonce
                )
            });
            OrderBinding {
                id: order.id.clone(),
                outcome: order.outcome.key().to_string(),
                candidate_commit: order.candidate_commit.clone(),
                candidate_id,
                source_sha256,
                verify_profiles,
                review_binding,
            }
        })
        .collect();

    let mut body = Envelope {
        schema_version: 1,
        envelope_sha256: String::new(),
        run_id: report.run_id.clone(),
        repo: report.repo.clone(),
        trusted_policy_sha256: report.trusted_policy_sha256.clone(),
        trusted_policy_identity: report.trusted_policy_identity.clone(),
        orders,
        integration_candidate: integration,
    };
    body.envelope_sha256 = digest_envelope(&body);
    body
}

fn digest_envelope(envelope: &Envelope) -> String {
    let mut hash = Sha256::new();
    hash.update(b"summoner.assurance-envelope.v1\0");
    // Hash a stable JSON projection without the self-digest field.
    let mut clone = serde_json::to_value(envelope).unwrap_or_default();
    if let Some(obj) = clone.as_object_mut() {
        obj.remove("envelope_sha256");
    }
    hash.update(serde_json::to_vec(&clone).unwrap_or_default());
    let mut hex = String::with_capacity(64);
    for byte in hash.finalize() {
        write!(&mut hex, "{byte:02x}").expect("hex");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{OrderReport, Outcome, RunReport};
    use std::collections::BTreeMap;

    #[test]
    fn envelope_digest_is_stable_for_same_report() {
        let mut orders = vec![OrderReport::new(
            &crate::order::Order {
                id: "a".into(),
                title: "t".into(),
                brief: "b".into(),
                scope: vec!["src".into()],
                acceptance: vec![],
                verify_profile: None,
                executor: None,
                reviewer: None,
                timeout_secs: None,
                max_tokens: None,
                base: None,
                branch: None,
                after: vec![],
                variants: vec![],
                claim_group: None,
                variant_of: None,
                source: "a.toml".into(),
            },
            "fake".into(),
        )];
        orders[0].outcome = Outcome::Verified;
        orders[0].candidate_commit = Some("abc".into());
        let report = RunReport::assemble(
            "run-1".into(),
            "/repo".into(),
            1,
            2,
            orders,
            Some("pol".into()),
            None,
        );
        let a = from_run(&report, None);
        let b = from_run(&report, None);
        assert_eq!(a.envelope_sha256, b.envelope_sha256);
        assert_eq!(a.envelope_sha256.len(), 64);
        assert_eq!(a.orders[0].candidate_commit.as_deref(), Some("abc"));
        let _ = BTreeMap::<String, u64>::new();
    }
}
