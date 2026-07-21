//! Prompt composition and candidate evidence for independent review.

use crate::grove::VerifySummary;
use crate::init::REVIEW_CHARTER;
use crate::order::Order;
use sha2::{Digest, Sha256};
use std::fmt::Write;

const DIFF_INLINE_CAP: usize = 96 * 1024;

pub struct Evidence<'a> {
    pub base: &'a str,
    pub diff: &'a str,
    pub diff_stat: &'a str,
    pub uncommitted: &'a str,
    pub tripwires: &'a [String],
    pub verify: &'a [VerifySummary],
}

pub fn compose_prompt(order: &Order, evidence: &Evidence<'_>, protocol: &str) -> String {
    let mut prompt = String::from(REVIEW_CHARTER);
    prompt.push_str(&format!("\n# Order {}: {}\n", order.id, order.title));
    prompt.push_str("\nScope the implementer was allowed to change:\n");
    for entry in &order.scope {
        prompt.push_str(&format!("- {entry}\n"));
    }
    prompt.push_str("\nAcceptance criteria (the definition of done):\n");
    if order.acceptance.is_empty() {
        prompt.push_str("- The brief below.\n");
    } else {
        for criterion in &order.acceptance {
            prompt.push_str(&format!("- {criterion}\n"));
        }
    }
    prompt.push_str("\n## Brief given to the implementer\n\n");
    prompt.push_str(&order.brief);
    prompt.push_str("\n\n## Verification evidence\n");
    if evidence.verify.is_empty() {
        prompt.push_str("- no verification profile ran\n");
    }
    for summary in evidence.verify {
        let result = if summary.passed { "passed" } else { "FAILED" };
        prompt.push_str(&format!("- profile {:?}: {result}\n", summary.profile));
    }
    prompt.push_str("\n## Uncommitted state (part of what you are judging)\n");
    if evidence.uncommitted.trim().is_empty() {
        prompt.push_str("- working tree clean\n");
    } else {
        prompt.push_str("```\n");
        prompt.push_str(evidence.uncommitted);
        prompt.push_str("\n```\n");
    }
    prompt.push_str("\n## Tripwires (deterministic anomaly indicators)\n");
    if evidence.tripwires.is_empty() {
        prompt.push_str("- none\n");
    }
    for flag in evidence.tripwires {
        prompt.push_str(&format!("- {flag}\n"));
    }
    prompt.push_str(&format!("\n## Diff since base {}\n\n", evidence.base));
    if evidence.diff.len() <= DIFF_INLINE_CAP {
        prompt.push_str("```diff\n");
        prompt.push_str(evidence.diff);
        prompt.push_str("\n```\n");
    } else {
        prompt.push_str(&format!(
            "The full diff is {} bytes; inspect it in the capsule.\n\n{}\n",
            evidence.diff.len(),
            evidence.diff_stat
        ));
    }
    prompt.push_str("\n## Required verdict protocol\n\n");
    prompt.push_str(protocol);
    prompt.push('\n');
    prompt
}

pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn order() -> Order {
        Order {
            id: "auth-fix".into(),
            title: "Fix token validation".into(),
            brief: "Do the thing.".into(),
            scope: vec!["src".into()],
            acceptance: vec!["tests pass".into()],
            verify_profile: None,
            executor: None,
            reviewer: None,
            timeout_secs: None,
            max_tokens: None,
            base: None,
            branch: None,
            variants: Vec::new(),
            claim_group: None,
            variant_of: None,
            after: Vec::new(),
            source: PathBuf::from("a.toml"),
        }
    }

    #[test]
    fn sha256_is_lowercase_fixed_width_hex() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn prompt_orders_charter_evidence_diff_and_protocol() {
        let evidence = Evidence {
            base: "abc",
            diff: "+fn x() {}",
            diff_stat: "1 file",
            uncommitted: "?? x",
            tripwires: &["assertion loss".into()],
            verify: &[],
        };
        let prompt = compose_prompt(&order(), &evidence, "BOUND-PROTOCOL");
        let points = [
            "# Review charter",
            "Do the thing.",
            "assertion loss",
            "+fn x()",
            "BOUND-PROTOCOL",
        ];
        let positions: Vec<_> = points
            .iter()
            .map(|point| prompt.find(point).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
