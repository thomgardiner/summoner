//! Path-overlap partition for the git host (no cargo topology).

use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn partition(sets: &Value) -> Value {
    let Some(arr) = sets.as_array() else {
        return json!({ "sets": [], "conflicts": [], "couplings": [], "waves": [] });
    };
    let mut conflicts = Vec::new();
    for (i, a) in arr.iter().enumerate() {
        for b in arr.iter().skip(i + 1) {
            let id_a = a["id"].as_str().unwrap_or("");
            let id_b = b["id"].as_str().unwrap_or("");
            let group_a = a["group"].as_str();
            let group_b = b["group"].as_str();
            if group_a.is_some() && group_a == group_b {
                continue;
            }
            let scope_a = scope_of(a);
            let scope_b = scope_of(b);
            if scopes_overlap(&scope_a, &scope_b) {
                conflicts.push(
                    json!({ "a": id_a, "b": id_b, "paths": intersection(&scope_a, &scope_b) }),
                );
            }
        }
    }
    let ids: Vec<String> = arr
        .iter()
        .filter_map(|s| s["id"].as_str().map(String::from))
        .collect();
    // Without cargo edges, each non-conflicting set is parallel; keep one wave
    // listing all ids — callers use conflicts + after for ordering.
    json!({
        "sets": arr,
        "conflicts": conflicts,
        "couplings": [],
        "waves": [ids],
    })
}

fn scope_of(set: &Value) -> Vec<String> {
    set["scope"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    for x in a {
        for y in b {
            if x == y || x.starts_with(&format!("{y}/")) || y.starts_with(&format!("{x}/")) {
                return true;
            }
        }
    }
    false
}

fn intersection(a: &[String], b: &[String]) -> Vec<String> {
    let sb: BTreeSet<_> = b.iter().cloned().collect();
    a.iter().filter(|x| sb.contains(*x)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_path_conflicts() {
        let sets = json!([
            { "id": "a", "scope": ["src"], "group": null },
            { "id": "b", "scope": ["src/lib.rs"], "group": null },
        ]);
        let p = partition(&sets);
        assert_eq!(p["conflicts"].as_array().unwrap().len(), 1);
    }
}
