//! Work orders: one file per delegated task, TOML or JSON by extension. The
//! orchestrator writes them; summoner never decomposes plans itself.

use crate::config::{Config, ExecutorBackend, PromptRouting};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Order {
    pub id: String,
    pub title: String,
    pub brief: String,
    /// Passed verbatim to `grove task begin --scope`; grove resolves
    /// `crate:<name>` entries itself, so `grove plan --json` claim_scopes paste in.
    pub scope: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<String>,
    pub verify_profile: Option<String>,
    pub executor: Option<String>,
    pub timeout_secs: Option<u64>,
    pub base: Option<String>,
    pub branch: Option<String>,
    /// Order ids that must reach `verified` or `completed` first. Ordering and
    /// failure propagation only: a dependent still builds from its own `base`,
    /// so an order that needs a dependency's changes says so explicitly with
    /// `base = "grove/smn-<dep-id>"` (branch names are deterministic).
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(skip)]
    pub source: PathBuf,
}

impl Order {
    /// The grove agent identity, which also fixes the default branch
    /// (`grove/smn-<id>`). The prefix is what `summoner status` filters on.
    pub fn agent(&self) -> String {
        format!("smn-{}", self.id)
    }

    pub fn executor_name(&self, config: &Config) -> Option<String> {
        self.executor.clone().or_else(|| config.default_executor())
    }
}

/// Load orders from files and directories. A directory contributes its
/// immediate `*.toml`/`*.json` entries sorted by name; nothing recursive.
pub fn load(paths: &[PathBuf]) -> Result<Vec<Order>> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(path)
                .with_context(|| format!("reading order directory {}", path.display()))?
            {
                // A dropped entry would silently dispatch a subset of the batch.
                let entry = entry
                    .with_context(|| format!("reading an entry of {}", path.display()))?
                    .path();
                if entry.is_file()
                    && entry
                        .extension()
                        .is_some_and(|ext| ext == "toml" || ext == "json")
                {
                    entries.push(entry);
                }
            }
            entries.sort();
            if entries.is_empty() {
                bail!(
                    "order directory {} contains no .toml or .json files",
                    path.display()
                );
            }
            files.extend(entries);
        } else {
            files.push(path.clone());
        }
    }
    if files.is_empty() {
        bail!("no order files given");
    }
    files.iter().map(|path| parse(path)).collect()
}

fn parse(path: &Path) -> Result<Order> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading order {}", path.display()))?;
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mut order: Order = match extension {
        "toml" => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        "json" => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
        }
        other => bail!(
            "{}: unsupported order extension {other:?} (want .toml or .json)",
            path.display()
        ),
    };
    // Absolute: executors receive {order_file} while running from the leased
    // worktree, where a caller-relative path would point at nothing.
    order.source = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    Ok(order)
}

/// Every problem in the batch, not just the first, so the orchestrator fixes
/// its order files in one pass instead of replaying the run per error.
pub fn validate(orders: &[Order], config: &Config) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut used_backends = BTreeSet::new();

    for order in orders {
        let at = order.source.display();
        let id_ok = !order.id.is_empty()
            && order
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !id_ok {
            problems.push(format!(
                "{at}: id {:?} must be non-empty [a-z0-9_-]+",
                order.id
            ));
        }
        if !seen_ids.insert(order.id.clone()) {
            problems.push(format!("{at}: duplicate id {:?}", order.id));
        }
        if order.title.trim().is_empty() {
            problems.push(format!("{at}: title is empty"));
        }
        if order.brief.trim().is_empty() {
            problems.push(format!("{at}: brief is empty"));
        }
        if order.scope.is_empty() || order.scope.iter().any(|s| s.trim().is_empty()) {
            problems.push(format!(
                "{at}: scope must be a non-empty list of non-empty entries"
            ));
        }
        if let Some(timeout) = order.timeout_secs
            && !(1..=604_800).contains(&timeout)
        {
            problems.push(format!(
                "{at}: timeout_secs must be between 1 and 604800 (7 days), got {timeout}"
            ));
        }
        match order.executor_name(config) {
            None => problems.push(format!(
                "{at}: no executor named and no default_executor configured"
            )),
            Some(name) => {
                if config.executors.contains_key(&name) {
                    used_backends.insert(name);
                } else {
                    problems.push(format!("{at}: executor {name:?} is not configured"));
                }
            }
        }
    }

    for order in orders {
        let at = order.source.display();
        for dep in &order.after {
            if dep == &order.id {
                problems.push(format!("{at}: after references the order itself"));
            } else if !seen_ids.contains(dep) {
                problems.push(format!("{at}: after references unknown order {dep:?}"));
            }
        }
    }
    let cycle = cycle_members(orders);
    if !cycle.is_empty() {
        problems.push(format!(
            "dependency cycle among orders: {}",
            cycle.join(", ")
        ));
    }

    for name in used_backends {
        problems.extend(backend_problems(&name, &config.executors[&name]));
    }
    problems
}

/// Kahn's elimination: whatever cannot be topologically drained is in (or
/// downstream of) a cycle. Unknown ids are reported separately and ignored here.
fn cycle_members(orders: &[Order]) -> Vec<String> {
    let ids: BTreeSet<&str> = orders.iter().map(|o| o.id.as_str()).collect();
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = orders
        .iter()
        .map(|order| {
            let wanted: BTreeSet<&str> = order
                .after
                .iter()
                .map(String::as_str)
                .filter(|dep| ids.contains(dep) && *dep != order.id)
                .collect();
            (order.id.as_str(), wanted)
        })
        .collect();
    loop {
        let ready: Vec<&str> = deps
            .iter()
            .filter(|(_, wanted)| wanted.is_empty())
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            break;
        }
        for id in ready {
            deps.remove(id);
            for wanted in deps.values_mut() {
                wanted.remove(id);
            }
        }
    }
    deps.keys().map(|id| id.to_string()).collect()
}

/// Routing and placeholders must agree, or the executor receives a literal
/// `{prompt}` string — or never receives the prompt at all.
fn backend_problems(name: &str, backend: &ExecutorBackend) -> Vec<String> {
    let has = |token: &str| backend.argv.iter().any(|arg| arg.contains(token));
    let mut problems = Vec::new();
    if backend.argv.is_empty() {
        problems.push(format!("executor {name:?}: argv is empty"));
        return problems;
    }
    match backend.prompt {
        PromptRouting::Arg if !has("{prompt}") => problems.push(format!(
            "executor {name:?}: prompt routing \"arg\" needs a {{prompt}} placeholder in argv"
        )),
        PromptRouting::File if !has("{prompt_file}") => problems.push(format!(
            "executor {name:?}: prompt routing \"file\" needs a {{prompt_file}} placeholder in argv"
        )),
        _ => {}
    }
    if backend.prompt != PromptRouting::Arg && has("{prompt}") {
        problems.push(format!(
            "executor {name:?}: argv references {{prompt}} but routing is not \"arg\""
        ));
    }
    if backend.prompt != PromptRouting::File && has("{prompt_file}") {
        problems.push(format!(
            "executor {name:?}: argv references {{prompt_file}} but routing is not \"file\""
        ));
    }
    if let Some(timeout) = backend.timeout_secs
        && !(1..=604_800).contains(&timeout)
    {
        problems.push(format!(
            "executor {name:?}: timeout_secs must be between 1 and 604800 (7 days), got {timeout}"
        ));
    }
    problems
}

/// Identical scope strings across orders are not an error (grove serializes
/// them: the later `task begin` reports a conflict and the order lands as
/// blocked), but they are almost always an orchestrator mistake worth naming
/// before worktrees are spent on them.
pub fn warnings(orders: &[Order]) -> Vec<String> {
    let mut first_owner: BTreeMap<&str, &str> = BTreeMap::new();
    let mut warnings = Vec::new();
    for order in orders {
        for entry in &order.scope {
            match first_owner.get(entry.as_str()) {
                Some(owner) => warnings.push(format!(
                    "orders {owner:?} and {:?} both claim scope {entry:?}; the later one will block",
                    order.id
                )),
                None => {
                    first_owner.insert(entry, &order.id);
                }
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptRouting;

    fn config_with(default: Option<&str>, backends: &[(&str, &[&str], PromptRouting)]) -> Config {
        let mut config = Config {
            default_executor: default.map(|s| s.to_string()),
            ..Config::default()
        };
        for (name, argv, routing) in backends {
            config.executors.insert(
                name.to_string(),
                ExecutorBackend {
                    argv: argv.iter().map(|s| s.to_string()).collect(),
                    prompt: *routing,
                    timeout_secs: None,
                    env_required: Vec::new(),
                    usage_marker: None,
                },
            );
        }
        config
    }

    fn write_order(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    const GOOD_TOML: &str = r#"
id = "auth-fix"
title = "Fix token validation"
brief = "Do the thing."
scope = ["crate:auth-core"]
acceptance = ["tests pass"]
"#;

    #[test]
    fn toml_and_json_orders_parse_and_directories_expand_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write_order(dir.path(), "b.toml", GOOD_TOML);
        write_order(
            dir.path(),
            "a.json",
            r#"{"id":"json-one","title":"t","brief":"b","scope":["src/lib.rs"]}"#,
        );
        write_order(dir.path(), "notes.md", "ignored");

        let orders = load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].id, "json-one");
        assert_eq!(orders[1].id, "auth-fix");
        assert_eq!(orders[1].agent(), "smn-auth-fix");
        assert!(orders[1].source.ends_with("b.toml"));
    }

    #[test]
    fn unknown_fields_reject_the_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_order(
            dir.path(),
            "typo.toml",
            "id = \"x\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"s\"]\nscop = [\"typo\"]\n",
        );
        assert!(load(&[path]).is_err());
    }

    #[test]
    fn validate_reports_every_problem_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_order(
            dir.path(),
            "a.toml",
            "id = \"Bad ID\"\ntitle = \"\"\nbrief = \"b\"\nscope = []\n",
        );
        let b = write_order(dir.path(), "b.toml", GOOD_TOML);
        let c = write_order(
            dir.path(),
            "c.toml",
            "id = \"auth-fix\"\ntitle = \"dup\"\nbrief = \"b\"\nscope = [\"x\"]\nexecutor = \"ghost\"\n",
        );
        let orders = load(&[a, b, c]).unwrap();
        let config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );

        let problems = validate(&orders, &config);
        let text = problems.join("\n");
        assert!(text.contains("must be non-empty [a-z0-9_-]+"), "{text}");
        assert!(text.contains("title is empty"), "{text}");
        assert!(text.contains("scope must be a non-empty list"), "{text}");
        assert!(text.contains("duplicate id"), "{text}");
        assert!(
            text.contains("executor \"ghost\" is not configured"),
            "{text}"
        );
    }

    #[test]
    fn missing_default_executor_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_order(dir.path(), "a.toml", GOOD_TOML);
        let orders = load(&[path]).unwrap();
        let problems = validate(&orders, &config_with(None, &[]));
        assert!(problems[0].contains("no executor named and no default_executor"));
    }

    #[test]
    fn routing_and_placeholders_must_agree_in_both_directions() {
        let arg_without_prompt = config_with(Some("x"), &[("x", &["run"], PromptRouting::Arg)]);
        let stdin_with_prompt = config_with(
            Some("x"),
            &[("x", &["run", "{prompt}"], PromptRouting::Stdin)],
        );
        let file_without_placeholder =
            config_with(Some("x"), &[("x", &["run"], PromptRouting::File)]);

        let dir = tempfile::tempdir().unwrap();
        let path = write_order(dir.path(), "a.toml", GOOD_TOML);
        let orders = load(&[path]).unwrap();

        assert!(validate(&orders, &arg_without_prompt)[0].contains("needs a {prompt} placeholder"));
        assert!(validate(&orders, &stdin_with_prompt)[0].contains("but routing is not \"arg\""));
        assert!(
            validate(&orders, &file_without_placeholder)[0]
                .contains("needs a {prompt_file} placeholder")
        );
    }

    #[test]
    fn timeouts_outside_the_sane_range_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );
        // TOML integers cap at i64::MAX; that is still far past the range gate.
        let path = write_order(
            dir.path(),
            "huge.toml",
            "id = \"huge\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\ntimeout_secs = 9223372036854775807\n",
        );
        let problems = validate(&load(&[path]).unwrap(), &config);
        assert!(
            problems[0].contains("timeout_secs must be between"),
            "{problems:?}"
        );

        let path = write_order(
            dir.path(),
            "zero.toml",
            "id = \"zero\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\ntimeout_secs = 0\n",
        );
        let problems = validate(&load(&[path]).unwrap(), &config);
        assert!(
            problems[0].contains("timeout_secs must be between"),
            "{problems:?}"
        );
    }

    #[test]
    fn after_must_reference_known_orders_without_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );

        let a = write_order(
            dir.path(),
            "a.toml",
            "id = \"a\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/a.rs\"]\nafter = [\"ghost\", \"a\"]\n",
        );
        let problems = validate(&load(&[a]).unwrap(), &config);
        let text = problems.join("\n");
        assert!(
            text.contains("references unknown order \"ghost\""),
            "{text}"
        );
        assert!(text.contains("references the order itself"), "{text}");

        let a = write_order(
            dir.path(),
            "cyc-a.toml",
            "id = \"a\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/a.rs\"]\nafter = [\"b\"]\n",
        );
        let b = write_order(
            dir.path(),
            "cyc-b.toml",
            "id = \"b\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/b.rs\"]\nafter = [\"a\"]\n",
        );
        let problems = validate(&load(&[a, b]).unwrap(), &config);
        assert!(
            problems.iter().any(|p| p.contains("dependency cycle")),
            "{problems:?}"
        );

        let a = write_order(
            dir.path(),
            "ok-a.toml",
            "id = \"a\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/a.rs\"]\n",
        );
        let b = write_order(
            dir.path(),
            "ok-b.toml",
            "id = \"b\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/b.rs\"]\nafter = [\"a\"]\n",
        );
        assert!(validate(&load(&[a, b]).unwrap(), &config).is_empty());
    }

    #[test]
    fn overlapping_scopes_warn_but_do_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_order(dir.path(), "a.toml", GOOD_TOML);
        let b = write_order(
            dir.path(),
            "b.toml",
            "id = \"other\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:auth-core\"]\n",
        );
        let orders = load(&[a, b]).unwrap();
        let config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );

        assert!(validate(&orders, &config).is_empty());
        let warnings = warnings(&orders);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("both claim scope \"crate:auth-core\""));
    }
}
