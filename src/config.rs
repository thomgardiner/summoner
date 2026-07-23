//! Layered, model-neutral executor configuration.

#[path = "config_load.rs"]
mod load;
#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

pub(crate) use load::{GroveProfiles, grove_profiles};
pub use load::{global_path, load, select_profile};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;

thread_local! { static SELECTED_PROFILE: RefCell<Option<String>> = const { RefCell::new(None) }; }

pub fn selected_profile(selected: Option<&str>) {
    SELECTED_PROFILE.with(|profile| profile.replace(selected.map(String::from)));
}

pub fn profile() -> Option<String> {
    SELECTED_PROFILE.with(|profile| profile.borrow().clone())
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_executor: Option<String>,
    /// Executor name spawned as an independent reviewer after each order
    /// verifies. Orders override with `reviewer = "<name>"` or opt out with
    /// `reviewer = "none"`.
    pub default_reviewer: Option<String>,
    pub max_parallel: Option<usize>,
    pub default_verify_profile: Option<String>,
    pub order_timeout_secs: Option<u64>,
    pub grove_bin: Option<String>,
    /// Isolation/verification host. Default resolution: explicit kind, else
    /// legacy grove_bin, else `.grove.toml`+grove on PATH → grove, else git.
    pub host: Option<HostSettings>,
    /// Verification profiles for the git host (`[verification]` table).
    pub verification: Option<crate::host::VerificationConfig>,
    pub keep_failed_worktrees: Option<bool>,
    pub fail_fast: Option<usize>,
    pub revise: Option<usize>,
    pub run_token_budget: Option<u64>,
    /// Executor names whose CLI cannot prove authentication but whose health
    /// check the user has explicitly accepted. Repository config is forbidden
    /// from setting this personal trust decision.
    #[serde(default)]
    pub allow_unknown_auth: Vec<String>,
    pub executors: BTreeMap<String, ExecutorBackend>,
    pub profile: Option<String>,
    pub profiles: BTreeMap<String, Profile>,
    /// Run-wide acceptance bar the orchestrator publishes. Enforced during
    /// validation and scheduling; its digest is pinned in the run manifest and
    /// report so consumers can prove which bar gated the run.
    pub trusted_policy: Option<TrustedPolicy>,
    /// Command run when the fleet reaches a moment worth looking up from other
    /// work. A personal side-channel over the run journal, not part of the
    /// reproducible run inputs, so it is read from live config and never bound
    /// into the run manifest.
    #[serde(default)]
    pub notify: Notify,
    #[serde(skip)]
    pub(crate) frozen: bool,
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct HostSettings {
    /// `git` (default independence path) or `grove`.
    pub kind: Option<String>,
    /// Grove binary when kind = grove (defaults to `grove` / grove_bin).
    pub bin: Option<String>,
    /// Directory for git-host worktrees.
    pub worktree_root: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// Run per notable event (run finished, a non-green order, a review
    /// starting): the event's JSON line arrives on stdin and
    /// `SUMMONER_NOTIFY_TITLE`/`_BODY`/`_EVENT` in the environment. Empty
    /// disables it. One `notify-send`/`osascript` line is an OS notification;
    /// a `curl` reading stdin is a webhook.
    pub command: Vec<String>,
}

#[derive(Deserialize, Serialize, Default, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct TrustedPolicy {
    /// Every order must carry an independent reviewer; `reviewer = "none"` is refused.
    pub require_reviewer: bool,
    /// The reviewer's configured name must differ from the executor's. This
    /// compares names, nothing deeper: two aliases invoking the same binary
    /// and model satisfy it when identity is unset.
    pub distinct_reviewer_name: bool,
    /// Require executor and reviewer to declare different `identity` strings
    /// (provider/model provenance label). Both must set identity; equal values
    /// refuse. Closes the two-aliases-same-model hole.
    pub distinct_reviewer_identity: bool,
    /// Orders must select their verify_profile from this list (one-of
    /// allowlist). It does not make every listed profile run.
    pub allowed_profiles: Vec<String>,
    /// Profiles that must run for every order under this policy (in addition to
    /// the order's own verify_profile). Empty means no mandatory multi-profile.
    pub required_profiles: Vec<String>,
    /// Closed set of executor names orders may use; empty allows any configured.
    pub allowed_executors: Vec<String>,
    /// Closed set of reviewer names; empty allows any configured.
    pub allowed_reviewers: Vec<String>,
    /// Protected paths beyond the built-in verification contract files. A diff
    /// touching one caps the order at `unverified`, like the built-ins.
    pub protected_paths: Vec<String>,
    /// Let an unverified `completed` upstream satisfy `after` edges. Off by
    /// default: under a trusted policy a dependency chain is only as green as
    /// its weakest link, so unverified links must be accepted deliberately.
    pub completed_satisfies_dependencies: bool,
    /// When set, the resolved host kind must match (e.g. `"grove"`). Prevents
    /// silent fallback from exact-state Grove to the weaker Git host.
    pub required_host: Option<String>,
    /// Exact-state host capabilities that must be true. Empty means no
    /// capability pin beyond `required_host` / reviewer rules.
    pub required_capabilities: crate::host::RequiredHostCapabilities,
}

impl TrustedPolicy {
    /// Paths that always join the tripwire protected set under a trusted policy
    /// (judge inputs and supply-chain surface), plus any operator extras.
    pub fn effective_protected_paths(&self) -> Vec<String> {
        let mut paths = JUDGE_PROTECTED_PATHS
            .iter()
            .map(|p| (*p).to_string())
            .collect::<Vec<_>>();
        for path in &self.protected_paths {
            if !paths.iter().any(|existing| existing == path) {
                paths.push(path.clone());
            }
        }
        paths
    }

    /// Content address of the exact policy in force, for the manifest and report.
    pub fn sha256(&self) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;
        let mut hash = Sha256::new();
        hash.update(b"summoner.trusted-policy.v3\0");
        hash.update(serde_json::to_vec(self).expect("policy serializes"));
        let mut hex = String::with_capacity(64);
        for byte in hash.finalize() {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        hex
    }
}

/// Authority surfaces Crucible and Summoner judge inputs share. Always protected
/// when a trusted policy is active so an executor cannot weaken its own bar.
pub const JUDGE_PROTECTED_PATHS: &[&str] = &[
    ".crucible",
    "Cargo.lock",
    "checks",
    "hooks",
    ".github/workflows",
];

#[derive(Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub default_executor: Option<String>,
    pub default_reviewer: Option<String>,
    /// Environment variable whose presence selects this profile automatically,
    /// so any harness that exports an identifying variable can self-register
    /// without a code change. The built-in Claude Code and Codex detection
    /// keeps working for profiles that leave this unset.
    #[serde(default)]
    pub detect_env: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutorBackend {
    /// Literal program and arguments, expanded per order. Elements are never
    /// shell-joined, so vendor flag ordering survives verbatim.
    #[serde(default)]
    pub argv: Vec<String>,
    /// Absent means inherit, then defaults to argument routing.
    pub prompt: Option<PromptRouting>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub env_required: Vec<String>,
    pub usage_marker: Option<String>,
    pub session_marker: Option<String>,
    #[serde(default)]
    pub resume_argv: Vec<String>,
    /// Operator-declared provider/model identity (e.g. `openai:gpt-5`,
    /// `anthropic:opus`). Used by `distinct_reviewer_identity` so two config
    /// aliases of the same model cannot satisfy independence.
    #[serde(default)]
    pub identity: Option<String>,
    /// Immutable launch bindings populated from a run manifest. Config files
    /// cannot set these fields.
    #[serde(skip)]
    pub(crate) provenance: Option<crate::backend_provenance::Provenance>,
    #[serde(skip)]
    pub(crate) resume_provenance: Option<crate::backend_provenance::Provenance>,
}

impl ExecutorBackend {
    pub fn routing(&self) -> PromptRouting {
        self.prompt.unwrap_or_default()
    }
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum PromptRouting {
    #[default]
    Arg,
    Stdin,
    File,
}

#[derive(Serialize)]
pub struct Resolved {
    pub sources: Vec<String>,
    #[serde(skip)]
    pub selected_profile: Option<String>,
    #[serde(flatten)]
    pub config: Config,
}

impl Config {
    fn env<T>(&self, read: impl FnOnce() -> Option<T>) -> Option<T> {
        if self.frozen { None } else { read() }
    }

    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    pub fn default_executor(&self) -> Option<String> {
        self.env(|| {
            std::env::var("SUMMONER_DEFAULT_EXECUTOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| self.default_executor.clone())
    }

    pub fn default_reviewer(&self) -> Option<String> {
        self.env(|| {
            std::env::var("SUMMONER_DEFAULT_REVIEWER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| self.default_reviewer.clone())
    }

    pub fn max_parallel(&self) -> usize {
        self.env(|| {
            std::env::var("SUMMONER_MAX_PARALLEL")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .or(self.max_parallel)
        .filter(|count| *count > 0)
        .unwrap_or(2)
    }

    pub fn order_timeout_secs(&self) -> u64 {
        self.env(|| {
            std::env::var("SUMMONER_ORDER_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .or(self.order_timeout_secs)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(600)
    }

    pub fn grove_bin(&self) -> String {
        std::env::var("SUMMONER_GROVE_BIN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.grove_bin.clone())
            .unwrap_or_else(|| "grove".to_string())
    }

    pub fn keep_failed_worktrees(&self) -> bool {
        self.env(|| load::env_bool("SUMMONER_KEEP_FAILED_WORKTREES"))
            .or(self.keep_failed_worktrees)
            .unwrap_or(false)
    }

    pub fn fail_fast(&self) -> Option<usize> {
        self.env(|| {
            std::env::var("SUMMONER_FAIL_FAST")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .or(self.fail_fast)
        .filter(|count| *count > 0)
    }

    pub fn revise(&self) -> usize {
        self.env(|| {
            std::env::var("SUMMONER_REVISE")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .or(self.revise)
        .unwrap_or(0)
    }

    pub fn run_token_budget(&self) -> Option<u64> {
        self.env(|| {
            std::env::var("SUMMONER_RUN_TOKEN_BUDGET")
                .ok()
                .and_then(|value| value.parse().ok())
        })
        .or(self.run_token_budget)
        .filter(|tokens| *tokens > 0)
    }

    pub fn unknown_auth_allowed(&self, executor: &str) -> bool {
        self.allow_unknown_auth.iter().any(|name| name == executor)
    }
}

fn merge_backend(base: &mut ExecutorBackend, over: ExecutorBackend) {
    fn marker(base: &mut Option<String>, over: Option<String>) {
        *base = match over {
            Some(value) if value.is_empty() => None,
            Some(value) => Some(value),
            None => base.take(),
        };
    }
    if !over.argv.is_empty() {
        base.argv = over.argv;
    }
    if over.prompt.is_some() {
        base.prompt = over.prompt;
    }
    base.timeout_secs = over.timeout_secs.or(base.timeout_secs);
    if !over.env_required.is_empty() {
        base.env_required = over.env_required;
    }
    marker(&mut base.usage_marker, over.usage_marker);
    marker(&mut base.session_marker, over.session_marker);
    if !over.resume_argv.is_empty() {
        base.resume_argv = over.resume_argv;
    }
}

fn merge(base: &mut Config, over: Config) {
    base.default_executor = over.default_executor.or(base.default_executor.take());
    base.default_reviewer = over.default_reviewer.or(base.default_reviewer.take());
    base.max_parallel = over.max_parallel.or(base.max_parallel);
    base.default_verify_profile = over
        .default_verify_profile
        .or(base.default_verify_profile.take());
    base.order_timeout_secs = over.order_timeout_secs.or(base.order_timeout_secs);
    base.grove_bin = over.grove_bin.or(base.grove_bin.take());
    if over.host.is_some() {
        base.host = over.host;
    }
    if over.verification.is_some() {
        base.verification = over.verification;
    }
    base.keep_failed_worktrees = over.keep_failed_worktrees.or(base.keep_failed_worktrees);
    base.fail_fast = over.fail_fast.or(base.fail_fast);
    base.revise = over.revise.or(base.revise);
    base.run_token_budget = over.run_token_budget.or(base.run_token_budget);
    for name in over.allow_unknown_auth {
        if !base.allow_unknown_auth.contains(&name) {
            base.allow_unknown_auth.push(name);
        }
    }
    for (name, backend) in over.executors {
        match base.executors.entry(name) {
            std::collections::btree_map::Entry::Occupied(mut existing) => {
                merge_backend(existing.get_mut(), backend);
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(backend);
            }
        }
    }
    // Wholesale replacement: a policy is one coherent bar, never a field-wise
    // blend of two authors' intentions.
    base.trusted_policy = over.trusted_policy.or(base.trusted_policy.take());
    base.profile = over.profile.or(base.profile.take());
    for (name, profile) in over.profiles {
        match base.profiles.entry(name) {
            std::collections::btree_map::Entry::Occupied(mut existing) => {
                let existing = existing.get_mut();
                existing.default_executor = profile
                    .default_executor
                    .or(existing.default_executor.take());
                existing.default_reviewer = profile
                    .default_reviewer
                    .or(existing.default_reviewer.take());
                existing.detect_env = profile.detect_env.or(existing.detect_env.take());
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(profile);
            }
        }
    }
}
