use super::*;

#[test]
fn current_executable_has_bounded_auditable_provenance() {
    let executable = std::env::current_exe().unwrap();
    let cwd = Path::new(env!("CARGO_MANIFEST_DIR"));
    let captured = capture(executable.to_str().unwrap(), cwd).unwrap();
    assert_eq!(captured.binary_sha256.len(), 64);
    assert!(captured.version_output.is_empty());
    require_current(&captured, executable.to_str().unwrap(), cwd).unwrap();
}

#[test]
fn changed_binary_is_rejected_as_resume_drift() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(std::env::current_exe().unwrap().file_name().unwrap());
    std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
    let captured = capture(path.to_str().unwrap(), dir.path()).unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"drift")
        .unwrap();
    let error = require_current(&captured, path.to_str().unwrap(), dir.path()).unwrap_err();
    assert!(error.to_string().contains("binary drift"));
}

#[cfg(unix)]
#[test]
fn version_probe_does_not_wait_for_an_escaped_session_descendant() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("version-tree.sh");
    std::fs::write(
        &path,
        "#!/usr/bin/env python3\nimport os, time\nif os.fork() == 0:\n    os.setsid()\n    time.sleep(5)\n    os._exit(0)\nos._exit(0)\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let output = version(&path).unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(output.output.is_empty());
    assert!(output.exit == Some(0) || output.timed_out);
}
