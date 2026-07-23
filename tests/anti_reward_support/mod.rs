//! Git-host fixture for anti-reward tests.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const SUMMONER: &str = env!("CARGO_BIN_EXE_summoner");

pub struct GitEnv {
    root: TempDir,
    pub repo: PathBuf,
    config_home: PathBuf,
    cache: PathBuf,
    bin: PathBuf,
}

impl GitEnv {
    pub fn new() -> Self {
        let root = TempDir::new().expect("temp");
        let repo = root.path().join("repo");
        let config_home = root.path().join("cfg");
        let cache = root.path().join("cache");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("README.md"), "hello\n").unwrap();
        std::fs::write(repo.join("src/lib.txt"), "lib\n").unwrap();
        run(&repo, &["git", "init", "-q"]);
        run(&repo, &["git", "config", "user.email", "ar@test"]);
        run(&repo, &["git", "config", "user.name", "ar"]);
        run(&repo, &["git", "add", "-A"]);
        run(&repo, &["git", "commit", "-qm", "init"]);
        Self {
            root,
            repo,
            config_home,
            cache,
            bin,
        }
    }

    pub fn write_config(&self, body: &str) {
        let dir = self.config_home.join("summoner");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    pub fn write_order(&self, name: &str, body: &str) {
        std::fs::create_dir_all(self.repo.join("orders")).unwrap();
        std::fs::write(self.repo.join("orders").join(name), body).unwrap();
    }

    pub fn write_worker(&self, body: &str) -> PathBuf {
        let path = self.bin.join("smn-worker");
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    pub fn path_without_grove(&self) -> OsString {
        let mut parts = vec![self.bin.clone()];
        for system in ["/usr/bin", "/bin", "/usr/local/bin", "/opt/homebrew/bin"] {
            let p = PathBuf::from(system);
            if p.is_dir() {
                parts.push(p);
            }
        }
        std::env::join_paths(parts).expect("join path")
    }

    pub fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(SUMMONER)
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", self.path_without_grove())
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_CACHE_HOME", &self.cache)
            .env("HOME", self.root.path())
            .env_remove("SUMMONER_GROVE_BIN")
            .output()
            .expect("summoner")
    }

    pub fn run_report(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cmd(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_ne!(
            out.status.code(),
            Some(2),
            "infra failure (exit {:?})\nstdout={stdout}\nstderr={stderr}",
            out.status.code()
        );
        let json = stdout
            .find('{')
            .map(|i| &stdout[i..])
            .unwrap_or(stdout.as_ref());
        serde_json::from_str(json).unwrap_or_else(|e| {
            panic!("report json: {e}\nstdout={stdout}\nstderr={stderr}");
        })
    }

    pub fn git(&self, args: &[&str]) {
        let mut v = Vec::with_capacity(args.len() + 1);
        v.push("git");
        v.extend_from_slice(args);
        run(&self.repo, &v);
    }
}

fn run(dir: &Path, argv: &[&str]) {
    assert!(
        Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "{argv:?} failed"
    );
}
