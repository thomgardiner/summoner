//! Order validation tests.

use super::*;
use crate::config::{Config, ExecutorBackend, PromptRouting};
use crate::order::{load, warnings};
use std::path::{Path, PathBuf};

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
                identity: None,
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
    let file_without_placeholder = config_with(Some("x"), &[("x", &["run"], PromptRouting::File)]);

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
        ..Default::default()
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

    // Distinct identity refuses two backends that claim the same model.
    config.executors.get_mut("fake").unwrap().identity = Some("vendor:model-a".into());
    config.executors.get_mut("judge").unwrap().identity = Some("vendor:model-a".into());
    config
        .trusted_policy
        .as_mut()
        .unwrap()
        .distinct_reviewer_identity = true;
    let same_id = write_order(
        dir.path(),
        "same-id.toml",
        "id = \"sid\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\n\
             executor = \"fake\"\nreviewer = \"judge\"\nverify_profile = \"full\"\n",
    );
    let text = validate(&load(&[same_id]).unwrap(), &config).join("\n");
    assert!(text.contains("distinct reviewer identity"), "{text}");
    config.executors.get_mut("judge").unwrap().identity = Some("vendor:model-b".into());
    config
        .trusted_policy
        .as_mut()
        .unwrap()
        .distinct_reviewer_identity = false;

    // A compliant order validates clean under the same policy.
    let good = write_order(
        dir.path(),
        "good.toml",
        "id = \"good\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\n\
             executor = \"fake\"\nreviewer = \"judge\"\nverify_profile = \"full\"\n",
    );
    assert!(validate(&load(&[good]).unwrap(), &config).is_empty());
}
