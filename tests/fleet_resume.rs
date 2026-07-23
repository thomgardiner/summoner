//! Fleet integration against the exact Grove release.
#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;
use common::*;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn resume_deduplicates_variant_siblings_sharing_one_order_file() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn f() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w",
        60,
    );
    let script = fixture.base.path().join("fake2-executor.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'pub fn g() {}' >> src/lib.rs\ngit add -A\ngit commit -qm w2\n",
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
title = "t"
brief = "b"
scope = ["src"]
verify_profile = "fast"
variants = ["fake", "fake2"]
"#,
    );

    let report = fixture.run_report(&[&order], 0);
    let run_id = report["run_id"].as_str().unwrap();

    // Both siblings report the same order file; resume must not load it
    // twice (duplicate expanded ids would abort validation).
    let output = fixture.summoner(&["resume", run_id]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let resumed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(resumed["summary"]["verified"], 2, "{resumed}");
    for entry in resumed["orders"].as_array().unwrap() {
        assert!(
            entry["detail"]
                .as_str()
                .is_some_and(|d| d.contains("carried")),
            "{resumed}"
        );
    }
}


#[test]
fn resume_carries_successes_and_reruns_failures_on_their_branches() {
    require_grove();
    let fixture = Fixture::new(true);
    // Order "bad" fails until the flag file exists; "good" always succeeds.
    let flag = fixture.base.path().join("fixed");
    fixture.executor(
        &format!(
            "branch=$(git symbolic-ref --short HEAD)\n\
             case \"$branch\" in\n\
               *smn-bad) if [ ! -f {flag} ]; then echo 'SESSION=bad-session'; exit 3; fi\n\
                         test \"$1\" = --resume && test \"$2\" = bad-session || exit 4\n\
                         echo 'pub fn bad() {{}}' > src/bad.rs ;;\n\
               *) echo 'pub fn good() {{}}' > src/good.rs ;;\n\
             esac\n\
             git add -A\ngit commit -qm 'executor work'",
            flag = flag.display()
        ),
        60,
    );
    fixture.append_config(&format!(
        "session_marker = \"SESSION=\"\nresume_argv = [\"{}\", \"--resume\", \"{{session_id}}\"]",
        fixture.base.path().join("fake-executor.sh").display()
    ));
    let a = fixture.order(
        "a.toml",
        "id = \"good\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/good.rs\"]\nverify_profile = \"fast\"\n",
    );
    let b = fixture.order(
        "b.toml",
        "id = \"bad\"\ntitle = \"t\"\nbrief = \"b\"\nscope = [\"src/bad.rs\"]\nverify_profile = \"fast\"\n",
    );

    let first = fixture.run_report(&[&a, &b], 1);
    assert_eq!(first["summary"]["verified"], 1, "{first}");
    assert_eq!(first["summary"]["executor_failed"], 1, "{first}");
    let run_id = first["run_id"].as_str().unwrap();

    std::fs::write(&flag, "").unwrap();
    // Recovery owns its inputs: remove both source orders and replace the
    // current executor/defaults with a backend that would fail if consulted.
    std::fs::remove_file(&a).unwrap();
    std::fs::remove_file(&b).unwrap();
    std::fs::write(
        fixture.repo.join(".summoner.toml"),
        "default_executor = \"poison\"\nmax_parallel = 9\n\n\
         [executors.poison]\nargv = [\"false\"]\nprompt = \"stdin\"\n",
    )
    .unwrap();
    fixture.commit_all("replace current summoner config");
    let output = fixture.summoner_with_env(
        &["resume", run_id],
        &[("SUMMONER_MAX_PARALLEL", "7"), ("SUMMONER_REVISE", "4")],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let second: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_resume_carry_report(&fixture, &second);
}


fn assert_resume_carry_report(fixture: &Fixture, second: &Value) {
    assert_eq!(second["summary"]["verified"], 2, "{second}");
    let good = second["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "good")
        .unwrap();
    assert!(
        good["detail"]
            .as_str()
            .is_some_and(|d| d.contains("carried from run")),
        "{second}"
    );
    let bad = second["orders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "bad")
        .unwrap();
    assert_eq!(bad["session_id"], "bad-session", "{second}");
    assert_eq!(bad["attempts"], 2, "{second}");
    let show = Command::new("git")
        .args(["show", "grove/smn-bad:src/bad.rs"])
        .current_dir(&fixture.repo)
        .output()
        .unwrap();
    assert!(show.status.success(), "{second}");
    let resumed_id = second["run_id"].as_str().unwrap();
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(
            fixture
                .base
                .path()
                .join("xdg/summoner/runs")
                .join(resumed_id)
                .join("manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["settings"]["max_parallel"], 2, "{manifest}");
    assert_eq!(manifest["settings"]["revise"], 0, "{manifest}");
    assert!(manifest["backends"].get("fake").is_some(), "{manifest}");
    assert!(manifest["backends"].get("poison").is_none(), "{manifest}");
    for order in manifest["orders"].as_array().unwrap() {
        assert_eq!(order["roles"]["executor"], "fake", "{manifest}");
        assert!(
            order["source_path"]
                .as_str()
                .is_some_and(|path| path.contains("resume-orders")),
            "{manifest}"
        );
    }
}


#[test]
fn resume_fails_closed_when_the_recorded_executor_binary_drifts() {
    require_grove();
    let fixture = Fixture::new(true);
    fixture.executor(
        "echo 'pub fn wave() {}' >> src/lib.rs\ngit add -A\ngit commit -qm 'executor work'",
        60,
    );
    let order = fixture.order("wave.toml", ORDER_TOML);
    let first = fixture.run_report(&[&order], 0);
    let run_id = first["run_id"].as_str().unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.base.path().join("fake-executor.sh"))
        .unwrap()
        .write_all(b"\n# upgraded\n")
        .unwrap();

    let resumed = fixture.summoner(&["resume", run_id]);
    assert_eq!(resumed.status.code(), Some(2));
    let error = String::from_utf8_lossy(&resumed.stderr);
    assert!(error.contains("executor binary drift"), "{error}");
    assert!(error.contains("start a new run"), "{error}");
}


