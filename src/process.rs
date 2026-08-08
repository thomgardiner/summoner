use crate::report::CommandEvidence;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::Once;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
static SIGNALS: Once = Once::new();
#[cfg(unix)]
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn run(
    mut command: Command,
    argv: Vec<String>,
    cwd: &Path,
    timeout_secs: u64,
    prompt: Option<&[u8]>,
    stdout_log: &Path,
    stderr_log: &Path,
) -> Result<CommandEvidence> {
    if argv.is_empty() {
        bail!("empty command");
    }
    let stdout = std::fs::File::create(stdout_log)
        .with_context(|| format!("creating {}", stdout_log.display()))?;
    let stderr = std::fs::File::create(stderr_log)
        .with_context(|| format!("creating {}", stderr_log.display()))?;
    command
        .current_dir(cwd)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .stdin(if prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        install_signals();
        command.process_group(0);
    }
    let started = Instant::now();
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning {}", argv[0]))?;
    if let Some(data) = prompt
        && let Some(mut stdin) = child.stdin.take()
    {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.clamp(1, 31_536_000));
    let (exit, timed_out, interrupted) = loop {
        let wait_for = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        match rx.recv_timeout(wait_for) {
            Ok(status) => {
                let status = match status {
                    Ok(status) => status,
                    Err(error) => {
                        kill_tree(pid);
                        return Err(error).context("waiting for child");
                    }
                };
                let interrupted = interrupted();
                kill_tree(pid);
                break (status.code(), false, interrupted);
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                kill_tree(pid);
                let _ = rx.recv_timeout(Duration::from_secs(2));
                let interrupted = interrupted();
                break (None, !interrupted, interrupted);
            }
            Err(mpsc::RecvTimeoutError::Timeout) if interrupted() => {
                kill_tree(pid);
                let _ = rx.recv_timeout(Duration::from_secs(2));
                break (None, false, true);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                kill_tree(pid);
                bail!("child waiter disconnected");
            }
        }
    };
    Ok(CommandEvidence {
        argv,
        exit,
        timed_out,
        interrupted,
        success: !timed_out && !interrupted && exit == Some(0),
        duration_ms: started.elapsed().as_millis(),
        stdout_log: stdout_log.display().to_string(),
        stderr_log: stderr_log.display().to_string(),
    })
}

fn interrupted() -> bool {
    #[cfg(unix)]
    {
        INTERRUPTED.load(Ordering::SeqCst)
    }
    #[cfg(not(unix))]
    false
}

#[cfg(unix)]
fn install_signals() {
    SIGNALS.call_once(|| unsafe {
        let handler = signal_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    });
}

#[cfg(unix)]
extern "C" fn signal_handler(_signal: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

pub fn executable_available(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.contains(std::path::MAIN_SEPARATOR) || Path::new(program).is_absolute() {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let candidate = dir.join(program);
            candidate.is_file()
                || (cfg!(windows)
                    && [".exe", ".cmd", ".bat"]
                        .iter()
                        .any(|suffix| dir.join(format!("{program}{suffix}")).is_file()))
        })
    })
}
