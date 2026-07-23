//! Verification profiles for the git host (from Summoner config).

use crate::config::Config;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerificationConfig {
    pub required: Vec<String>,
    pub profiles: std::collections::BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub argv: Vec<String>,
    #[serde(default)]
    pub allow_zero_tests: bool,
}

/// Load from optional `[verification]` on Config via serde re-parse of a sidecar
/// is not available; wire through Config.verification once present. Until then,
/// return empty (finish may allow_unverified when nothing is required).
pub fn load(config: &Config) -> VerificationConfig {
    config.verification.clone().unwrap_or_default()
}
