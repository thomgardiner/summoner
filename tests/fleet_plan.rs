//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn variants_race_the_same_scope_and_both_land_on_their_own_branches() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn from_fake() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'fake work'",
        60,
    );
    // A second backend racing the same order: same scope, different output.
    let script = fixture.base.path().join("fake2-executor.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'pub fn from_fake2() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'fake2 work'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.append_config(&format!(
        "\n[executors.fake2]\nargv = [\"{}\"]\nprompt = \"stdin\"\ntimeout_secs = 60",
        script.display()
    ));
    let order = fixture.order(
        "race.toml",
        r#"
id = "race"
title = "Race two executors over one order"
brief = "Append a function to src/lib.rs and commit."
scope = ["src"]
verify_profile = "fast"
variants = ["fake", "fake2"]
"#,
    );

    let report = fixture.run_report(&[&order], 0);
    assert_eq!(report["summary"]["verified"], 2, "{report}");
    for (entry, executor) in report["orders"]
        .as_array()
        .unwrap()
        .iter()
        .zip(["fake", "fake2"])
    {
        assert_eq!(entry["id"], format!("race-{executor}"), "{report}");
        assert_eq!(entry["outcome"], "verified", "{report}");
        assert_eq!(entry["executor"], executor, "{report}");
        assert_eq!(entry["variant_of"], "race", "{report}");
        assert_eq!(
            entry["branch"],
            format!("grove/smn-race-{executor}"),
            "{report}"
        );
    }

    // Each attempt survives on its own branch; the orchestrator lands a winner.
    for (executor, marker) in [("fake", "from_fake()"), ("fake2", "from_fake2()")] {
        let show = Command::new("git")
            .args(["show", &format!("grove/smn-race-{executor}:src/lib.rs")])
            .current_dir(&fixture.repo)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&show.stdout).contains(marker),
            "branch for {executor} missing {marker}"
        );
    }

    let mut states = fixture.task_states();
    states.sort();
    assert_eq!(
        states,
        [
            ("smn-race-fake".into(), "finished".into()),
            ("smn-race-fake2".into(), "finished".into()),
        ]
    );
}


#[test]
fn plan_refutes_a_batch_then_passes_it_once_revised() {
    require_grove();
    let (base, repo) = two_crate_workspace();
    std::fs::write(
        repo.join(".summoner.toml"),
        "default_executor = \"fake\"\n\
         [executors.fake]\nargv = [\"true\"]\nprompt = \"stdin\"\ntimeout_secs = 60\n\
         [executors.fake2]\nargv = [\"true\"]\nprompt = \"stdin\"\ntimeout_secs = 60\n",
    )
    .unwrap();
    let orders = base.path().join("orders");
    std::fs::create_dir_all(&orders).unwrap();
    std::fs::write(
        orders.join("core.toml"),
        "id = \"core-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:core\"]\n",
    )
    .unwrap();
    std::fs::write(
        orders.join("app.toml"),
        "id = \"app-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:app\"]\n",
    )
    .unwrap();

    let plan = |expect_exit: i32| -> Value {
        let output = Command::new(SUMMONER)
            .args(["plan", orders.to_str().unwrap()])
            .current_dir(&repo)
            .env("SUMMONER_GROVE_BIN", grove_bin())
            .env("GROVE_CACHE_ROOT", base.path().join("cache"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expect_exit),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    // Package topology is useful context, but disjoint work orders may run in
    // parallel even when one package depends on the other.
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    assert!(report["missing_after"].is_null(), "{report}");
    assert_eq!(
        report["partition"]["couplings"][0]["kind"], "dependency",
        "{report}"
    );

    // Declare the suggested edge: clean, with a two-wave schedule.
    std::fs::write(
        orders.join("app.toml"),
        "id = \"app-work\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crate:app\"]\nafter = [\"core-work\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    assert_eq!(
        report["partition"]["waves"],
        serde_json::json!([["core-work"], ["app-work"]])
    );

    // A genuine claim conflict needs ordering.
    std::fs::write(
        orders.join("clash.toml"),
        "id = \"clash\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"crates/core/src\"]\n",
    )
    .unwrap();
    let report = plan(1);
    assert_eq!(report["verdict"], "revise", "{report}");
    assert!(
        report["partition"]["conflicts"]
            .as_array()
            .is_some_and(|c| !c.is_empty()),
        "{report}"
    );

    // Once explicitly serialized, the overlap is dispatchable.
    std::fs::write(
        orders.join("clash.toml"),
        "id = \"clash\"\ntitle = \"t\"\nbrief = \"b\"\n\
         scope = [\"crates/core/src\"]\nafter = [\"core-work\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");

    // Variant siblings deliberately share scope; their claim group flows
    // through the partition, so an N-version race plans clean.
    std::fs::write(
        orders.join("race.toml"),
        "id = \"race\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"README.md\"]\n\
         variants = [\"fake\", \"fake2\"]\n",
    )
    .unwrap();
    let report = plan(0);
    assert_eq!(report["verdict"], "clean", "{report}");
    // The ordered clash conflict remains listed; the point is the siblings'
    // deliberate overlap never registers as one.
    for conflict in report["partition"]["conflicts"].as_array().unwrap() {
        for side in ["a", "b"] {
            assert!(
                !conflict[side]
                    .as_str()
                    .is_some_and(|id| id.starts_with("race-")),
                "{report}"
            );
        }
    }
}


