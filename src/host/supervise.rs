//! Process-group / Job Object supervision for SummonerSupervised execution.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub struct SupervisedOutcome {
    pub exit: Option<i32>,
    pub backup_killed: bool,
}

pub fn run(
    argv: &[String],
    cwd: &Path,
    timeout_secs: u64,
    stdin: Option<&[u8]>,
    stdout: Stdio,
    stderr: Stdio,
    shutdown: &AtomicBool,
) -> Result<SupervisedOutcome> {
    if argv.is_empty() {
        bail!("empty supervised argv");
    }
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(stdout)
        .stderr(stderr);
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group so we can kill the whole tree.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        // Job Object assignment is best-effort via CREATE_NEW_PROCESS_GROUP;
        // full Job Object wiring can land later. Preflight may refuse Windows.
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning supervised {}", argv[0]))?;
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let data = bytes.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = pipe.write_all(&data);
        });
    }
    wait_with_timeout(&mut child, timeout_secs, shutdown)
}

fn wait_with_timeout(
    child: &mut Child,
    timeout_secs: u64,
    shutdown: &AtomicBool,
) -> Result<SupervisedOutcome> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.min(31_536_000));
    let mut terminated = false;
    loop {
        if let Some(status) = child.try_wait().context("waiting supervised child")? {
            return Ok(SupervisedOutcome {
                exit: status.code(),
                backup_killed: false,
            });
        }
        if (shutdown.load(Ordering::SeqCst) || Instant::now() >= deadline) && !terminated {
            kill_tree(child);
            terminated = true;
        }
        if terminated && let Some(status) = child.try_wait().context("reaping killed child")? {
            return Ok(SupervisedOutcome {
                exit: status.code().or(Some(124)),
                backup_killed: Instant::now() >= deadline,
            });
        }
        std::thread::sleep(Duration::from_millis(100));
        if terminated && Instant::now() >= deadline + Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(SupervisedOutcome {
                exit: None,
                backup_killed: true,
            });
        }
    }
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(200));
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = child.kill();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.kill();
    }
}
