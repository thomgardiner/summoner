use super::*;

fn backend(argv: &[&str]) -> ExecutorBackend {
    ExecutorBackend {
        argv: argv.iter().map(|value| value.to_string()).collect(),
        prompt: Some(PromptRouting::Arg),
        timeout_secs: None,
        env_required: Vec::new(),
        usage_marker: None,
        session_marker: None,
        resume_argv: Vec::new(),
        identity: None,
        provenance: None,
        resume_provenance: None,
    }
}

fn merged_config() -> Config {
    let mut base = Config {
        default_executor: Some("glm".into()),
        max_parallel: Some(4),
        ..Config::default()
    };
    base.executors.insert("glm".into(), backend(&["opencode"]));
    base.executors.insert("codex".into(), backend(&["codex"]));
    base.executors.get_mut("codex").unwrap().session_marker = Some("session id:".into());
    base.executors.get_mut("codex").unwrap().resume_argv = vec!["codex".into(), "resume".into()];
    base.profiles.insert(
        "claude".into(),
        Profile {
            default_executor: Some("codex".into()),
            default_reviewer: Some("codex-review".into()),
            detect_env: None,
        },
    );

    let mut over = Config {
        max_parallel: Some(2),
        keep_failed_worktrees: Some(true),
        ..Config::default()
    };
    let mut pinned = backend(&["codex", "exec", "-m", "terra"]);
    pinned.prompt = None;
    over.executors.insert("codex".into(), pinned);
    over.profiles.insert(
        "claude".into(),
        Profile {
            default_executor: None,
            default_reviewer: Some("glm-review".into()),
            detect_env: None,
        },
    );
    merge(&mut base, over);
    base
}

#[test]
fn repo_config_overrides_global_and_keeps_unset_globals() {
    let base = merged_config();
    assert_eq!(base.max_parallel, Some(2));
    assert_eq!(base.keep_failed_worktrees, Some(true));
    assert_eq!(base.default_executor.as_deref(), Some("glm"));
    assert_eq!(base.executors["glm"].argv, ["opencode"]);
    let codex = &base.executors["codex"];
    assert_eq!(codex.argv, ["codex", "exec", "-m", "terra"]);
    assert_eq!(codex.session_marker.as_deref(), Some("session id:"));
    assert_eq!(codex.resume_argv, ["codex", "resume"]);
}

#[test]
fn an_empty_marker_clears_the_inherited_value_only() {
    let mut base = merged_config();
    let mut muted = Config::default();
    let mut clear = backend(&[]);
    clear.prompt = None;
    clear.session_marker = Some(String::new());
    muted.executors.insert("codex".into(), clear);
    merge(&mut base, muted);
    assert_eq!(base.executors["codex"].session_marker, None);
    assert_eq!(
        base.profiles["claude"].default_reviewer.as_deref(),
        Some("glm-review")
    );
    assert_eq!(
        base.profiles["claude"].default_executor.as_deref(),
        Some("codex")
    );
}

#[test]
fn repo_config_is_found_from_a_subdirectory() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join(".summoner.toml"), "max_parallel = 7\n").unwrap();
    let deep = repo.path().join("src/nested");
    std::fs::create_dir_all(&deep).unwrap();
    let found = load::repo_config_path_from(&deep).expect("ancestor config");
    assert_eq!(load::read(&found).unwrap().unwrap().max_parallel, Some(7));
}

#[test]
fn unparseable_config_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".summoner.toml");
    std::fs::write(&path, "max_parallel = 2\nmax_paralel = 4\n").unwrap();
    let error = match load::read(&path) {
        Ok(_) => panic!("invalid config was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("parsing"), "{error:#}");
}

#[test]
fn unreadable_config_path_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::create_dir(&path).unwrap();
    let error = match load::read(&path) {
        Ok(_) => panic!("unreadable config was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("reading"), "{error:#}");
}

#[test]
fn repository_cannot_persist_an_authentication_acknowledgement() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".summoner.toml"),
        "allow_unknown_auth = [\"kimi\"]\n",
    )
    .unwrap();
    let error = match load::load_from(repo.path()) {
        Ok(_) => panic!("repository auth acknowledgement was accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("personal global config"),
        "{error:#}"
    );
}

#[test]
fn repository_cannot_publish_the_trusted_policy_that_gates_it() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join(".summoner.toml"),
        "[trusted_policy]\nrequire_reviewer = false\n",
    )
    .unwrap();
    let error = match load::load_from(repo.path()) {
        Ok(_) => panic!("repository published its own acceptance bar"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("cannot set trusted_policy"),
        "{error:#}"
    );
}

#[test]
fn a_trusted_policy_digest_changes_with_every_field_that_narrows_it() {
    let base = crate::config::TrustedPolicy::default();
    let baseline = base.sha256();
    assert_eq!(baseline.len(), 64);
    assert_eq!(baseline, crate::config::TrustedPolicy::default().sha256());

    let mut stricter = base.clone();
    stricter.require_reviewer = true;
    assert_ne!(stricter.sha256(), baseline);

    let mut narrowed = base.clone();
    narrowed.allowed_executors = vec!["codex".into()];
    assert_ne!(narrowed.sha256(), baseline);

    let mut protected = base.clone();
    protected.protected_paths = vec!["ci/verify.sh".into()];
    assert_ne!(protected.sha256(), baseline);

    let mut epoch = base.clone();
    epoch.policy_epoch = 2;
    assert_ne!(epoch.sha256(), baseline);
}

#[test]
fn policy_signature_is_excluded_from_digest_and_verifies_with_key() {
    let mut policy = crate::config::TrustedPolicy {
        policy_id: Some("org".into()),
        policy_epoch: 3,
        issuer: Some("security".into()),
        require_reviewer: true,
        ..Default::default()
    };
    let digest = policy.sha256();
    let key = b"test-operator-key";
    let mac = crate::config::TrustedPolicy::mac_hex(key, &digest);
    policy.signature = Some(mac.clone());
    // Signature must not alter the content digest of the body.
    assert_eq!(policy.sha256(), digest);

    // SAFETY: single-threaded test process; restore after.
    unsafe { std::env::set_var("SUMMONER_POLICY_KEY", "test-operator-key") };
    assert_eq!(policy.verify_signature().unwrap(), Some(true));

    policy.signature = Some("0".repeat(64));
    assert_eq!(policy.verify_signature().unwrap(), Some(false));
    policy.require_signature = true;
    assert!(
        policy
            .verify_signature()
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    unsafe { std::env::remove_var("SUMMONER_POLICY_KEY") };
}

#[test]
fn resume_floor_rejects_older_epochs() {
    let live = crate::config::TrustedPolicy {
        policy_epoch: 10,
        minimum_resumable_epoch: 7,
        ..Default::default()
    };
    assert!(live.allows_resume_of(7));
    assert!(live.allows_resume_of(10));
    assert!(!live.allows_resume_of(6));
}

#[test]
fn revoked_executor_is_refused_by_policy() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        default_executor: Some("codex".into()),
        ..Config::default()
    };
    config.executors.insert("codex".into(), backend(&["codex"]));
    config.trusted_policy = Some(crate::config::TrustedPolicy {
        revoked_executors: vec!["codex".into()],
        policy_epoch: 2,
        ..Default::default()
    });
    std::fs::write(
        dir.path().join("o.toml"),
        "id = \"o1\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src\"]\nexecutor = \"codex\"\n",
    )
    .unwrap();
    let orders = crate::order::load(&[dir.path().join("o.toml")]).unwrap();
    let problems = crate::order::validate(&orders, &config);
    assert!(
        problems.iter().any(|p| p.contains("revokes executor")),
        "{problems:?}"
    );
}

#[test]
fn executor_backend_parses_with_routing_and_defaults() {
    let cfg: Config = toml::from_str(
        r#"
        [executors.fake]
        argv = ["fake-agent", "{prompt}"]
        prompt = "stdin"
        env_required = ["FAKE_KEY"]
        "#,
    )
    .unwrap();
    assert_eq!(cfg.executors["fake"].routing(), PromptRouting::Stdin);
    assert_eq!(cfg.executors["fake"].env_required, ["FAKE_KEY"]);
    assert_eq!(cfg.executors["fake"].timeout_secs, None);
    let cfg: Config = toml::from_str("[executors.plain]\nargv = [\"plain\"]\n").unwrap();
    assert_eq!(cfg.executors["plain"].routing(), PromptRouting::Arg);
}

#[test]
fn profiles_overlay_defaults_and_reject_unknown_names() {
    let mut cfg: Config = toml::from_str(
        r#"
        default_executor = "glm"
        default_reviewer = "codex-review"
        [profiles.codex]
        default_reviewer = "claude-review"
        "#,
    )
    .unwrap();
    let name = select_profile(&mut cfg, Some("codex")).unwrap();
    assert_eq!(name.as_deref(), Some("codex"));
    assert_eq!(cfg.default_executor.as_deref(), Some("glm"));
    assert_eq!(cfg.default_reviewer.as_deref(), Some("claude-review"));
    let error = select_profile(&mut cfg, Some("ghost")).unwrap_err();
    assert!(error.to_string().contains("ghost"), "{error}");
}

#[test]
fn profile_pin_selects_and_flag_wins() {
    let text = r#"
        profile = "codex"
        [profiles.codex]
        default_reviewer = "claude-review"
        [profiles.claude]
        default_reviewer = "codex-review"
    "#;
    let mut cfg: Config = toml::from_str(text).unwrap();
    assert_eq!(
        select_profile(&mut cfg, None).unwrap().as_deref(),
        Some("codex")
    );
    let mut cfg: Config = toml::from_str(text).unwrap();
    assert_eq!(
        select_profile(&mut cfg, Some("claude")).unwrap().as_deref(),
        Some("claude")
    );
    let mut cfg: Config = toml::from_str("profile = \"ghost\"").unwrap();
    assert!(select_profile(&mut cfg, None).is_err());
}

#[test]
fn orchestrator_detection_is_conservative() {
    let env = |vars: &'static [&'static str]| move |name: &str| vars.contains(&name);
    assert_eq!(
        load::detect_orchestrator(env(&["CLAUDECODE"])),
        Some("claude")
    );
    assert_eq!(
        load::detect_orchestrator(env(&["CODEX_SANDBOX"])),
        Some("codex")
    );
    assert_eq!(
        load::detect_orchestrator(env(&["CLAUDECODE", "CODEX_SANDBOX"])),
        None
    );
    assert_eq!(load::detect_orchestrator(env(&["CODEX_HOME"])), None);
}

#[test]
fn defaults_apply_when_nothing_is_configured() {
    let cfg = Config::default();
    assert_eq!(cfg.max_parallel(), 2);
    assert_eq!(cfg.order_timeout_secs(), 600);
    assert_eq!(cfg.grove_bin(), "grove");
    assert!(!cfg.keep_failed_worktrees());
    assert_eq!(cfg.default_executor(), None);
}

/// Any harness that exports an identifying variable can self-register a
/// profile in config, with no vendor list compiled into summoner. Ambiguous
/// matches select nothing rather than guessing.
#[test]
fn detect_env_selects_a_profile_for_any_harness_and_refuses_ambiguity() {
    let mut config = Config::default();
    config.profiles.insert(
        "gemini".into(),
        Profile {
            default_executor: Some("gemini-exec".into()),
            default_reviewer: None,
            detect_env: Some("SUMMONER_TEST_GEMINI_MARKER".into()),
        },
    );
    unsafe { std::env::remove_var("SUMMONER_PROFILE") };
    unsafe { std::env::set_var("SUMMONER_TEST_GEMINI_MARKER", "1") };
    let selected = load::select_profile(&mut config, None).unwrap();
    assert_eq!(selected.as_deref(), Some("gemini"));
    assert_eq!(config.default_executor.as_deref(), Some("gemini-exec"));

    // A second matching profile makes detection ambiguous: none selected.
    config.profiles.insert(
        "other".into(),
        Profile {
            default_executor: None,
            default_reviewer: None,
            detect_env: Some("SUMMONER_TEST_GEMINI_MARKER".into()),
        },
    );
    let mut fresh = config.clone();
    fresh.default_executor = None;
    let selected = load::select_profile(&mut fresh, None).unwrap();
    assert_eq!(selected, None);
    unsafe { std::env::remove_var("SUMMONER_TEST_GEMINI_MARKER") };
}
