//! Resolved executable identity captured in immutable run evidence.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const VERSION_TIMEOUT: Duration = Duration::from_secs(3);
static SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Provenance {
    pub(crate) resolved_path: String,
    pub(crate) binary_sha256: String,
    pub(crate) version_output: String,
    pub(crate) version_exit: Option<i32>,
    pub(crate) version_timed_out: bool,
    pub(crate) version_truncated: bool,
}

pub(crate) fn capture(binary: &str, cwd: &Path) -> Result<Provenance> {
    let path = resolve(binary, cwd)?;
    let binary_sha256 = sha256(&path)?;
    let version = version(&path)?;
    Ok(Provenance {
        resolved_path: path.display().to_string(),
        binary_sha256,
        version_output: version.output,
        version_exit: version.exit,
        version_timed_out: version.timed_out,
        version_truncated: version.truncated,
    })
}

pub(crate) fn require_current(expected: &Provenance, binary: &str, cwd: &Path) -> Result<()> {
    let path = resolve(binary, cwd)?;
    require_exact(
        &expected.resolved_path,
        &expected.binary_sha256,
        path.to_str().context("executor path is not valid UTF-8")?,
    )
}

pub(crate) fn require_exact(
    expected_path: &str,
    expected_sha256: &str,
    binary: &str,
) -> Result<()> {
    let path = Path::new(binary)
        .canonicalize()
        .with_context(|| format!("canonicalizing executor binary {binary}"))?;
    let digest = sha256(&path)?;
    if path.display().to_string() != expected_path || digest != expected_sha256 {
        bail!(
            "executor binary drift: recorded {} ({}) but resolved {} ({}); start a new run",
            expected_path,
            expected_sha256,
            path.display(),
            digest
        )
    }
    Ok(())
}

fn resolve(binary: &str, cwd: &Path) -> Result<PathBuf> {
    let requested = Path::new(binary);
    if requested.is_absolute() || binary.contains('/') || binary.contains('\\') {
        let path = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            cwd.join(requested)
        };
        return resolve_candidate(&path, binary);
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .flat_map(|dir| candidates(&dir.join(binary)))
            .find(|path| executable(path))
    }) {
        return path
            .canonicalize()
            .with_context(|| format!("canonicalizing executor binary {}", path.display()));
    }
    bail!("executor binary {binary:?} is not on PATH")
}

fn resolve_candidate(path: &Path, binary: &str) -> Result<PathBuf> {
    let path = candidates(path)
        .into_iter()
        .find(|path| executable(path))
        .with_context(|| format!("executor binary {binary:?} is not executable"))?;
    path.canonicalize()
        .with_context(|| format!("canonicalizing executor binary {}", path.display()))
}

fn candidates(path: &Path) -> Vec<PathBuf> {
    if !cfg!(windows) || path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| path.with_extension(extension.trim_start_matches('.')))
        .collect()
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

struct Version {
    output: String,
    exit: Option<i32>,
    timed_out: bool,
    truncated: bool,
}

fn version(path: &Path) -> Result<Version> {
    let scratch = Scratch::new()?;
    let mut command = Command::new(path);
    command
        .arg("--version")
        .current_dir(&scratch.path)
        .stdin(Stdio::null())
        // Identity comes from the canonical path and digest. Discard vendor
        // diagnostics so an escaped descendant cannot retain a pipe or grow
        // a capture file after the bounded probe returns.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_tree(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("running {} --version", path.display()))?;
    let tree = ProcessTree::attach(&child)?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let (exit, timed_out) = wait(&mut child, &tree, deadline)?;
    tree.terminate();
    Ok(Version {
        output: String::new(),
        exit,
        timed_out,
        truncated: false,
    })
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Result<Self> {
        for _ in 0..100 {
            let id = SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("summoner-version-{}-{id}", std::process::id()));
            match private_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating executor version scratch"),
            }
        }
        bail!("could not allocate executor version scratch directory")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening executor binary {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading executor binary {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let mut hex = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(hex)
}

fn wait(
    child: &mut std::process::Child,
    tree: &ProcessTree,
    deadline: Instant,
) -> Result<(Option<i32>, bool)> {
    loop {
        if let Some(status) = child.try_wait().context("waiting for executor version")? {
            return Ok((status.code(), false));
        }
        if Instant::now() >= deadline {
            tree.terminate();
            let status = child.wait().context("reaping executor version")?;
            return Ok((status.code(), true));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn configure_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_tree(_command: &mut Command) {}

struct ProcessTree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ProcessTree {
    #[cfg(not(windows))]
    fn attach(child: &std::process::Child) -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            pid: child.id(),
        })
    }

    #[cfg(windows)]
    fn attach(child: &std::process::Child) -> Result<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                bail!(
                    "creating executor version job object: {}",
                    std::io::Error::last_os_error()
                )
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, child.id());
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                CloseHandle(job);
                bail!(
                    "opening executor version process: {}",
                    std::io::Error::last_os_error()
                )
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                CloseHandle(job);
                bail!(
                    "assigning executor version job object: {}",
                    std::io::Error::last_os_error()
                )
            }
            Ok(Self { job })
        }
    }

    #[cfg(unix)]
    fn terminate(&self) {
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
    }

    #[cfg(not(any(unix, windows)))]
    fn terminate(&self) {}
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(test)]
#[path = "backend_provenance_tests.rs"]
mod tests;
