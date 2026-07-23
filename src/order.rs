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
    /// Executor name spawned as an independent reviewer after this order
    /// verifies; overrides `default_reviewer`. `"none"` opts this order out.
    pub reviewer: Option<String>,
    pub timeout_secs: Option<u64>,
    /// Token ceiling for this order. Usage is scraped after each executor
    /// exit, so this cannot stop a live overrun — but an over-budget attempt
    /// is never revised, and the overage is called out in the report.
    pub max_tokens: Option<u64>,
    pub base: Option<String>,
    pub branch: Option<String>,
    /// Order ids that must finish first. This orders dispatch AND supplies the
    /// base: a dependent branches from its dependencies' verified commits, so
    /// their code is present without naming a base by hand. One dependency is
    /// inherited directly, several are merged, and dependencies that conflict
    /// skip the order rather than starting it on a tree missing half its
    /// inputs. An explicit `base` still wins.
    #[serde(default)]
    pub after: Vec<String>,
    /// N-version dispatch: executor names that each attempt this order
    /// independently. The order expands into one sibling per executor
    /// (`<id>-<executor>`), all sharing a grove claim group so they may
    /// overlap the same scope; the orchestrator reviews and lands one winner.
    #[serde(default)]
    pub variants: Vec<String>,
    /// Internal: the grove claim group variant siblings share.
    #[serde(skip)]
    pub claim_group: Option<String>,
    /// Internal: the original order id a variant sibling was expanded from.
    #[serde(skip)]
    pub variant_of: Option<String>,
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

    /// The reviewer to gate this order with, after config defaults and the
    /// `"none"` opt-out. None means the order ships ungated.
    pub fn reviewer_name(&self, config: &Config) -> Option<String> {
        match self.reviewer.as_deref() {
            Some("none") => None,
            Some(name) => Some(name.to_string()),
            None => config.default_reviewer(),
        }
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
    let orders: Vec<Order> = files
        .iter()
        .map(|path| parse(path))
        .collect::<Result<_>>()?;
    Ok(expand_variants(orders))
}

/// One sibling per variant executor, sharing a claim group so the deliberate
/// scope overlap does not conflict. Only one sibling's branch will land; the
/// orchestrator picks it during review.
fn expand_variants(orders: Vec<Order>) -> Vec<Order> {
    orders
        .into_iter()
        .flat_map(|order| {
            // An explicit executor alongside variants is ambiguous; leave it
            // unexpanded so validation names the problem.
            if order.variants.is_empty() || order.executor.is_some() {
                return vec![order];
            }
            order
                .variants
                .clone()
                .into_iter()
                .map(|executor| {
                    let mut sibling = order.clone();
                    sibling.id = format!("{}-{}", order.id, executor);
                    sibling.executor = Some(executor);
                    sibling.claim_group = Some(order.id.clone());
                    sibling.variant_of = Some(order.id.clone());
                    sibling.variants = Vec::new();
                    sibling
                })
                .collect()
        })
        .collect()
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

/// Only when the operator *opts in* to the git host. Auto-resolved git
/// (no grove present) still gets the check; explicit grove does not.
fn git_host_active(config: &Config) -> bool {
    if let Some(host) = &config.host
        && let Some(kind) = host.kind.as_deref()
    {
        return kind.eq_ignore_ascii_case("git");
    }
    // No explicit host: use resolver (git when no grove / no .grove.toml).
    let repo = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::host::resolve(config, &repo).kind == "git"
}

/// Every problem in the batch, not just the first, so the orchestrator fixes
/// its order files in one pass instead of replaying the run per error.
pub fn validate(orders: &[Order], config: &Config) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen_ids = BTreeSet::new();
    let mut used_backends = BTreeSet::new();

    for order in orders {
        problems.extend(validate_order_fields(
            order,
            config,
            &mut seen_ids,
            &mut used_backends,
        ));
    }

    let expanded_ids: BTreeSet<&str> = orders
        .iter()
        .filter_map(|o| o.variant_of.as_deref())
        .collect();
    for order in orders {
        let at = order.source.display();
        for dep in &order.after {
            if dep == &order.id {
                problems.push(format!("{at}: after references the order itself"));
            } else if expanded_ids.contains(dep.as_str()) {
                problems.push(format!(
                    "{at}: after references {dep:?}, which expanded into variants; \
                     name a specific sibling such as \"{dep}-<executor>\""
                ));
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

fn validate_order_fields(
    order: &Order,
    config: &Config,
    seen_ids: &mut BTreeSet<String>,
    used_backends: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
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
    // crate: claims need Cargo topology (grove host). On a resolved git
    // host they are opaque and unsafe to treat as path claims.
    if git_host_active(config) {
        for entry in &order.scope {
            if entry.starts_with("crate:") {
                problems.push(format!(
                    "{at}: scope entry {entry:?} uses crate: which requires the grove host; set [host] kind = \"grove\" or use filesystem paths"
                ));
            }
        }
    }
    if let Some(timeout) = order.timeout_secs
        && !(1..=604_800).contains(&timeout)
    {
        problems.push(format!(
            "{at}: timeout_secs must be between 1 and 604800 (7 days), got {timeout}"
        ));
    }
    // Expansion replaced variants orders with siblings; anything still
    // carrying both fields set them together, which is ambiguous.
    if !order.variants.is_empty() {
        problems.push(format!(
            "{at}: variants and executor are mutually exclusive (variants pick executors)"
        ));
    }
    match order.executor_name(config) {
        None => problems.push(format!(
            "{at}: no executor named and no default_executor configured"
        )),
        Some(name) => match config.executors.get(&name) {
            Some(backend) => {
                if order.max_tokens.is_some() && backend.usage_marker.is_none() {
                    problems.push(format!(
                        "{at}: max_tokens needs executor {name:?} to define a \
                         usage_marker, or the cap can never be measured"
                    ));
                }
                used_backends.insert(name);
            }
            None => problems.push(format!("{at}: executor {name:?} is not configured")),
        },
    }
    if let Some(name) = order.reviewer_name(config) {
        if config.executors.contains_key(&name) {
            used_backends.insert(name);
        } else {
            problems.push(format!("{at}: reviewer {name:?} is not configured"));
        }
    }
    if let Some(policy) = config.trusted_policy.as_ref() {
        problems.extend(
            policy_problems(order, config, policy)
                .into_iter()
                .map(|problem| format!("{at}: {problem}")),
        );
    }
    problems
}

/// The trusted policy's per-order demands. Unknown executors and reviewers are
/// already reported by the base validation, so absence here means "resolve
/// failed" and stays quiet rather than doubling up.
fn policy_problems(
    order: &Order,
    config: &Config,
    policy: &crate::config::TrustedPolicy,
) -> Vec<String> {
    let mut problems = Vec::new();
    let reviewer = order.reviewer_name(config);
    if policy.require_reviewer && reviewer.is_none() {
        problems.push("trusted policy requires an independent reviewer".to_string());
    }
    if let (Some(reviewer), Some(executor)) = (reviewer.as_deref(), order.executor_name(config)) {
        if policy.distinct_reviewer_name && reviewer == executor {
            problems.push(format!(
                "trusted policy requires a reviewer distinct from executor {executor:?}"
            ));
        }
        if !policy.allowed_reviewers.is_empty()
            && !policy.allowed_reviewers.iter().any(|name| name == reviewer)
        {
            problems.push(format!(
                "trusted policy does not allow reviewer {reviewer:?} (allowed: {})",
                policy.allowed_reviewers.join(", ")
            ));
        }
    }
    if let Some(executor) = order.executor_name(config)
        && !policy.allowed_executors.is_empty()
        && !policy
            .allowed_executors
            .iter()
            .any(|name| name == &executor)
    {
        problems.push(format!(
            "trusted policy does not allow executor {executor:?} (allowed: {})",
            policy.allowed_executors.join(", ")
        ));
    }
    if !policy.allowed_profiles.is_empty() {
        let profile = order
            .verify_profile
            .clone()
            .or_else(|| config.default_verify_profile.clone());
        let allowed = profile
            .as_deref()
            .is_some_and(|name| policy.allowed_profiles.iter().any(|p| p == name));
        if !allowed {
            problems.push(format!(
                "trusted policy requires a verify_profile from [{}], got {}",
                policy.allowed_profiles.join(", "),
                profile.as_deref().unwrap_or("none")
            ));
        }
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

/// Whether `downstream` is transitively ordered after `upstream`.
pub(crate) fn depends_on(orders: &[Order], downstream: &str, upstream: &str) -> bool {
    let by_id: BTreeMap<&str, &Order> = orders
        .iter()
        .map(|order| (order.id.as_str(), order))
        .collect();
    let mut pending = vec![downstream];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        let Some(order) = by_id.get(id) else {
            continue;
        };
        for dependency in &order.after {
            if dependency == upstream {
                return true;
            }
            if seen.insert(dependency.as_str()) {
                pending.push(dependency);
            }
        }
    }
    false
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
    match backend.routing() {
        PromptRouting::Arg if !has("{prompt}") => problems.push(format!(
            "executor {name:?}: prompt routing \"arg\" needs a {{prompt}} placeholder in argv"
        )),
        PromptRouting::File if !has("{prompt_file}") => problems.push(format!(
            "executor {name:?}: prompt routing \"file\" needs a {{prompt_file}} placeholder in argv"
        )),
        _ => {}
    }
    if backend.routing() != PromptRouting::Arg && has("{prompt}") {
        problems.push(format!(
            "executor {name:?}: argv references {{prompt}} but routing is not \"arg\""
        ));
    }
    if backend.routing() != PromptRouting::File && has("{prompt_file}") {
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
    // A resume template quietly suppresses the full charter, so it must
    // provably resume the right session and deliver the revision evidence.
    if !backend.resume_argv.is_empty() {
        let has_resume = |token: &str| backend.resume_argv.iter().any(|arg| arg.contains(token));
        if backend.session_marker.is_none() {
            problems.push(format!(
                "executor {name:?}: resume_argv needs a session_marker to capture the \
                 session it resumes"
            ));
        }
        if !has_resume("{session_id}") {
            problems.push(format!(
                "executor {name:?}: resume_argv needs a {{session_id}} placeholder"
            ));
        }
        match backend.routing() {
            PromptRouting::Arg if !has_resume("{prompt}") => problems.push(format!(
                "executor {name:?}: resume_argv needs a {{prompt}} placeholder \
                 (routing \"arg\")"
            )),
            PromptRouting::File if !has_resume("{prompt_file}") => problems.push(format!(
                "executor {name:?}: resume_argv needs a {{prompt_file}} placeholder \
                 (routing \"file\")"
            )),
            _ => {}
        }
    }
    problems
}

/// Identical scope strings across orders are not an error (grove serializes
/// them: the later `task begin` reports a conflict and the order lands as
/// blocked), but they are almost always an orchestrator mistake worth naming
/// before worktrees are spent on them. Variant siblings share a claim group,
/// so their deliberate overlap is not a mistake and stays quiet.
pub fn warnings(orders: &[Order], config: &Config) -> Vec<String> {
    let mut first_owner: BTreeMap<&str, &Order> = BTreeMap::new();
    let mut warnings = Vec::new();
    for order in orders {
        // A backend judging its own vendor's work loses the fresh-eyes
        // independence the gate exists for; different vendors catch more.
        if let (Some(reviewer), Some(executor)) =
            (order.reviewer_name(config), order.executor_name(config))
            && reviewer == executor
        {
            warnings.push(format!(
                "order {:?}: reviewer and executor are both {executor:?}; \
                 an independent review wants a different backend",
                order.id
            ));
        }
    }
    for order in orders {
        for entry in &order.scope {
            match first_owner.get(entry.as_str()) {
                Some(owner)
                    if owner.claim_group.is_none() || owner.claim_group != order.claim_group =>
                {
                    if !depends_on(orders, &order.id, &owner.id)
                        && !depends_on(orders, &owner.id, &order.id)
                    {
                        warnings.push(format!(
                            "orders {:?} and {:?} both claim scope {entry:?}; \
                             add an after edge so they do not block",
                            owner.id, order.id
                        ));
                    }
                }
                Some(_) => {}
                None => {
                    first_owner.insert(entry, order);
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
            // Explicit grove so unit tests using crate: scopes are not failed by
            // auto-git resolve when the checkout has no grove on PATH (CI unit job).
            host: Some(crate::config::HostSettings {
                kind: Some("grove".into()),
                bin: None,
                worktree_root: None,
            }),
            ..Config::default()
        };
        for (name, argv, routing) in backends {
            config.executors.insert(
                name.to_string(),
                ExecutorBackend {
                    argv: argv.iter().map(|s| s.to_string()).collect(),
                    prompt: Some(*routing),
                    timeout_secs: None,
                    env_required: Vec::new(),
                    usage_marker: None,
                    session_marker: None,
                    resume_argv: Vec::new(),
                    provenance: None,
                    resume_provenance: None,
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
        // Path scope (not crate:) so auto-git host resolution doesn't surface
        // a crate:-on-git problem ahead of the executor check under test.
        let path = write_order(
            dir.path(),
            "a.toml",
            r#"
id = "auth-fix"
title = "Fix token validation"
brief = "Do the thing."
scope = ["src/auth.rs"]
acceptance = ["tests pass"]
"#,
        );
        let orders = load(&[path]).unwrap();
        let problems = validate(&orders, &config_with(None, &[]));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("no executor named and no default_executor")),
            "problems={problems:?}"
        );
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
    fn resume_templates_and_token_caps_must_be_measurable() {
        let dir = tempfile::tempdir().unwrap();

        // resume_argv without a session_marker, a {session_id}, or prompt
        // delivery is a misconfigured continuation, named field by field.
        let mut config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );
        let backend = config.executors.get_mut("fake").unwrap();
        backend.resume_argv = vec!["fake".into(), "resume".into()];
        let path = write_order(dir.path(), "a.toml", GOOD_TOML);
        let orders = load(std::slice::from_ref(&path)).unwrap();
        let text = validate(&orders, &config).join("\n");
        assert!(text.contains("needs a session_marker"), "{text}");
        assert!(text.contains("{session_id} placeholder"), "{text}");
        assert!(
            text.contains("resume_argv needs a {prompt} placeholder"),
            "{text}"
        );

        let backend = config.executors.get_mut("fake").unwrap();
        backend.session_marker = Some("session id:".into());
        backend.resume_argv = vec![
            "fake".into(),
            "resume".into(),
            "{session_id}".into(),
            "{prompt}".into(),
        ];
        assert!(validate(&orders, &config).is_empty());

        // max_tokens without a usage_marker can never be measured.
        let capped = write_order(
            dir.path(),
            "capped.toml",
            "id = \"capped\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nmax_tokens = 1000\n",
        );
        let problems = validate(&load(&[capped]).unwrap(), &config);
        assert!(
            problems.iter().any(|p| p.contains("usage_marker")),
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
    fn reviewer_resolution_validation_and_same_backend_warning() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with(
            Some("fake"),
            &[
                ("fake", &["fake", "{prompt}"], PromptRouting::Arg),
                ("judge", &["judge", "{prompt}"], PromptRouting::Arg),
            ],
        );

        // Unknown reviewer is a validation problem.
        let path = write_order(
            dir.path(),
            "ghost.toml",
            "id = \"g\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nreviewer = \"ghost\"\n",
        );
        let problems = validate(&load(&[path]).unwrap(), &config);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("reviewer \"ghost\" is not configured")),
            "{problems:?}"
        );

        // default_reviewer applies; "none" opts out; both validate clean.
        config.default_reviewer = Some("judge".into());
        let gated = write_order(dir.path(), "a.toml", GOOD_TOML);
        let opted_out = write_order(
            dir.path(),
            "b.toml",
            "id = \"solo\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"x\"]\nreviewer = \"none\"\n",
        );
        let orders = load(&[gated, opted_out]).unwrap();
        assert_eq!(orders[0].reviewer_name(&config).as_deref(), Some("judge"));
        assert_eq!(orders[1].reviewer_name(&config), None);
        assert!(validate(&orders, &config).is_empty());
        assert!(warnings(&orders, &config).is_empty());

        // Reviewer == executor loses independence: warned, not refused.
        config.default_reviewer = Some("fake".into());
        let warned = warnings(&orders, &config);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(warned[0].contains("reviewer and executor are both \"fake\""));
    }

    #[test]
    fn variants_expand_into_siblings_sharing_a_claim_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_order(
            dir.path(),
            "race.toml",
            "id = \"race\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/lib.rs\"]\nvariants = [\"fake\", \"fake2\"]\n",
        );
        let orders = load(&[path]).unwrap();
        assert_eq!(orders.len(), 2);
        for (order, executor) in orders.iter().zip(["fake", "fake2"]) {
            assert_eq!(order.id, format!("race-{executor}"));
            assert_eq!(order.executor.as_deref(), Some(executor));
            assert_eq!(order.claim_group.as_deref(), Some("race"));
            assert_eq!(order.variant_of.as_deref(), Some("race"));
            assert!(order.variants.is_empty());
        }
        let config = config_with(
            None,
            &[
                ("fake", &["fake", "{prompt}"], PromptRouting::Arg),
                ("fake2", &["fake2", "{prompt}"], PromptRouting::Arg),
            ],
        );
        assert!(validate(&orders, &config).is_empty());
        // The siblings' identical scope is deliberate; no overlap warning.
        assert!(warnings(&orders, &config).is_empty());
    }

    #[test]
    fn variants_alongside_an_executor_are_rejected_not_expanded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_order(
            dir.path(),
            "both.toml",
            "id = \"both\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nexecutor = \"fake\"\nvariants = [\"fake\", \"fake2\"]\n",
        );
        let orders = load(&[path]).unwrap();
        assert_eq!(orders.len(), 1);
        let config = config_with(
            None,
            &[
                ("fake", &["fake", "{prompt}"], PromptRouting::Arg),
                ("fake2", &["fake2", "{prompt}"], PromptRouting::Arg),
            ],
        );
        let problems = validate(&orders, &config);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("variants and executor are mutually exclusive")),
            "{problems:?}"
        );
    }

    #[test]
    fn after_naming_an_expanded_original_id_gets_a_specific_hint() {
        let dir = tempfile::tempdir().unwrap();
        let race = write_order(
            dir.path(),
            "race.toml",
            "id = \"race\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/a.rs\"]\nvariants = [\"fake\", \"fake2\"]\n",
        );
        let dep = write_order(
            dir.path(),
            "dep.toml",
            "id = \"dep\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/b.rs\"]\nafter = [\"race\"]\n",
        );
        let config = config_with(
            Some("fake"),
            &[
                ("fake", &["fake", "{prompt}"], PromptRouting::Arg),
                ("fake2", &["fake2", "{prompt}"], PromptRouting::Arg),
            ],
        );
        let problems = validate(&load(&[race, dep]).unwrap(), &config);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("expanded into variants")),
            "{problems:?}"
        );
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
        let warnings = warnings(&orders, &config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("both claim scope \"crate:auth-core\""));
    }

    #[test]
    fn ordered_overlapping_scopes_do_not_warn() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_order(dir.path(), "a.toml", GOOD_TOML);
        let b = write_order(
            dir.path(),
            "b.toml",
            "id = \"other\"\ntitle = \"t\"\nbrief = \"b\"\n\
             scope = [\"crate:auth-core\"]\nafter = [\"auth-fix\"]\n",
        );
        let orders = load(&[a, b]).unwrap();
        let config = config_with(
            Some("fake"),
            &[("fake", &["fake", "{prompt}"], PromptRouting::Arg)],
        );

        assert!(validate(&orders, &config).is_empty());
        assert!(warnings(&orders, &config).is_empty());
    }

    #[test]
    fn a_trusted_policy_refuses_ungated_orders_and_disallowed_backends() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_with(
            Some("fake"),
            &[
                ("fake", &["fake", "{prompt}"], PromptRouting::Arg),
                ("judge", &["judge", "{prompt}"], PromptRouting::Arg),
                ("stranger", &["stranger", "{prompt}"], PromptRouting::Arg),
            ],
        );
        config.trusted_policy = Some(crate::config::TrustedPolicy {
            require_reviewer: true,
            distinct_reviewer_name: true,
            allowed_profiles: vec!["full".into()],
            allowed_executors: vec!["fake".into()],
            allowed_reviewers: vec!["judge".into()],
            protected_paths: Vec::new(),
            completed_satisfies_dependencies: false,
        });

        // Ungated, wrong profile: every demand is named in one pass.
        let ungated = write_order(
            dir.path(),
            "ungated.toml",
            "id = \"ungated\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nreviewer = \"none\"\n",
        );
        let text = validate(&load(&[ungated]).unwrap(), &config).join("\n");
        assert!(text.contains("requires an independent reviewer"), "{text}");
        assert!(
            text.contains("requires a verify_profile from [full]"),
            "{text}"
        );

        // Reviewer equal to executor, and neither backend on the allow lists.
        let same = write_order(
            dir.path(),
            "same.toml",
            "id = \"same\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\n\
             executor = \"stranger\"\nreviewer = \"stranger\"\nverify_profile = \"full\"\n",
        );
        let text = validate(&load(&[same]).unwrap(), &config).join("\n");
        assert!(text.contains("reviewer distinct from executor"), "{text}");
        assert!(
            text.contains("does not allow executor \"stranger\""),
            "{text}"
        );
        assert!(
            text.contains("does not allow reviewer \"stranger\""),
            "{text}"
        );

        // A compliant order validates clean under the same policy.
        let good = write_order(
            dir.path(),
            "good.toml",
            "id = \"good\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\n\
             executor = \"fake\"\nreviewer = \"judge\"\nverify_profile = \"full\"\n",
        );
        assert!(validate(&load(&[good]).unwrap(), &config).is_empty());
    }
}
