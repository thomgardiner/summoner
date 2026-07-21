use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

const MAX_PROMPT_BYTES: usize = 1024 * 1024;

pub fn run(
    prompt_file: &std::path::Path,
    stdin: bool,
    expected_path: &str,
    expected_sha256: &str,
    expected_prompt_sha256: &str,
    command: &[String],
) -> Result<i32> {
    let mut prompt = Vec::new();
    std::fs::File::open(prompt_file)
        .context("opening review prompt")?
        .take((MAX_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut prompt)
        .context("reading review prompt")?;
    if prompt.len() > MAX_PROMPT_BYTES {
        bail!("review prompt exceeds {MAX_PROMPT_BYTES} bytes")
    }
    let actual_prompt_sha256 = crate::review::sha256(&prompt);
    if actual_prompt_sha256 != expected_prompt_sha256 {
        bail!(
            "review prompt digest mismatch: expected {expected_prompt_sha256}, found {actual_prompt_sha256}"
        )
    }
    let (program, args) = command
        .split_first()
        .context("review worker requires a command")?;
    crate::backend_provenance::require_exact(expected_path, expected_sha256, program)
        .context("validating reviewer binary immediately before launch")?;
    let mut child = Command::new(program);
    child.args(args);
    if stdin {
        child.stdin(Stdio::piped());
    } else {
        child.stdin(Stdio::null());
    }
    for name in std::env::vars_os().map(|(name, _)| name) {
        let text = name.to_string_lossy();
        if text.starts_with("GROVE_")
            || matches!(
                text.as_ref(),
                "CARGO_TARGET_DIR" | "CARGO_BUILD_BUILD_DIR" | "MAKEFLAGS" | "SUMMONER_GROVE_BIN"
            )
        {
            child.env_remove(name);
        }
    }
    let mut child = child
        .spawn()
        .with_context(|| format!("spawning reviewer {program}"))?;
    if stdin {
        child
            .stdin
            .take()
            .context("reviewer stdin unavailable")?
            .write_all(&prompt)
            .context("writing verified review prompt")?;
    }
    let status = child.wait().context("waiting for reviewer")?;
    match status.code() {
        Some(code) => Ok(code),
        None => bail!("reviewer terminated by signal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;

    #[test]
    fn changed_reviewer_is_rejected_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = dir.path().join("prompt.md");
        std::fs::write(&prompt, "").unwrap();
        let binary = dir
            .path()
            .join(std::env::current_exe().unwrap().file_name().unwrap());
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        let expected = crate::backend_provenance::capture(
            binary.to_str().unwrap(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&binary)
            .unwrap()
            .write_all(b"drift")
            .unwrap();
        let error = run(
            &prompt,
            false,
            &expected.resolved_path,
            &expected.binary_sha256,
            &crate::review::sha256(b""),
            &[binary.display().to_string()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("validating reviewer binary"));
    }

    #[test]
    fn mutated_prompt_is_rejected_before_reviewer_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let prompt = dir.path().join("prompt.md");
        std::fs::write(&prompt, "trusted").unwrap();
        let expected = crate::review::sha256(b"trusted");
        std::fs::write(&prompt, "mutated").unwrap();
        let executable = std::env::current_exe().unwrap();
        let provenance = crate::backend_provenance::capture(
            executable.to_str().unwrap(),
            Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .unwrap();
        let error = run(
            &prompt,
            false,
            &provenance.resolved_path,
            &provenance.binary_sha256,
            &expected,
            &[executable.display().to_string()],
        )
        .unwrap_err();
        assert!(error.to_string().contains("review prompt digest mismatch"));
    }
}
