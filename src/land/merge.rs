//! Merge candidate commits onto the integration branch.

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

use super::git::git;
use super::Candidate;

pub(crate) fn merge_candidates(repo: &Path, order: &[Candidate]) -> (Vec<Value>, Option<Value>) {
    let mut landed = Vec::new();
    for candidate in order {
        let commit = candidate.commit.as_deref().expect("landable has a commit");
        if git(repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_err() {
            return (
                landed,
                Some(json!({
                    "id": candidate.id,
                    "reason": format!("candidate commit {commit} is missing from the repository"),
                })),
            );
        }
        match merge(repo, &candidate.id, commit) {
            Ok(mode) => landed.push(json!({"id": candidate.id, "commit": commit, "mode": mode})),
            Err(conflict) => {
                let _ = git(repo, &["merge", "--abort"]);
                return (
                    landed,
                    Some(json!({"id": candidate.id, "commit": commit, "reason": conflict})),
                );
            }
        }
    }
    (landed, None)
}


pub(crate) fn merge(repo: &Path, id: &str, commit: &str) -> Result<&'static str, String> {
    let output = Command::new("git")
        .args([
            "merge",
            "--no-edit",
            "-m",
            &format!("summoner: land order {id} ({commit})"),
            commit,
        ])
        .current_dir(repo)
        .output()
        .map_err(|error| format!("running git merge: {error}"))?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(if text.contains("Fast-forward") {
            "fast-forward"
        } else {
            "merge"
        })
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("merge failed")
            .trim()
            .to_string())
    }
}
