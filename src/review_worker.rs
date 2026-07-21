use anyhow::{Context, Result, bail};
use std::fs::File;
use std::process::{Command, Stdio};

pub fn run(prompt_file: &std::path::Path, stdin: bool, command: &[String]) -> Result<i32> {
    let (program, args) = command
        .split_first()
        .context("review worker requires a command")?;
    let mut child = Command::new(program);
    child.args(args);
    if stdin {
        child.stdin(Stdio::from(
            File::open(prompt_file).context("opening review prompt")?,
        ));
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
    let status = child
        .status()
        .with_context(|| format!("spawning reviewer {program}"))?;
    match status.code() {
        Some(code) => Ok(code),
        None => bail!("reviewer terminated by signal"),
    }
}
