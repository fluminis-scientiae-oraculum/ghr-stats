//! Runtime configuration loaded from a TOML file.
//!
//! *Where* the file lives and *where* data is written is decided by
//! [`crate::paths`] (privilege-scoped). This module owns only the config
//! schema, its defaults, and read-only token resolution. Every field has a
//! default, so the tool runs with no config at all.

mod secret;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use secret::Secret;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// SQLite database path.
    #[serde(default = "defaults::db_path")]
    pub db_path: PathBuf,

    /// Append-only NDJSON job-event log written by the runner hooks.
    #[serde(default = "defaults::event_log")]
    pub event_log: PathBuf,

    /// Roots scanned for runner install dirs (each contains a `.runner` file).
    /// Empty by default — set via `ghr-stats config` (no host-specific guess).
    #[serde(default = "defaults::runner_roots")]
    pub runner_roots: Vec<PathBuf>,

    /// GitHub orgs to reconcile against the API. Empty ⇒ derived from the orgs
    /// discovered in `.runner` files.
    #[serde(default)]
    pub orgs: Vec<String>,

    #[serde(default)]
    pub intervals: Intervals,

    #[serde(default)]
    pub github: GithubConfig,

    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intervals {
    /// Local source sampling cadence (runner discovery / processes / host).
    #[serde(default = "defaults::local_secs")]
    pub local_secs: u64,
    /// GitHub API polling cadence (rate-limit aware).
    #[serde(default = "defaults::api_secs")]
    pub api_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// Fallback token for any org without a specific one below. Prefer per-org
    /// `tokens`; `GHR_STATS_GITHUB_TOKEN` (env) overrides this fallback.
    #[serde(default)]
    pub token: Option<Secret>,
    /// Per-org fine-grained read-only PATs: org login → token.
    #[serde(default)]
    pub tokens: BTreeMap<String, Secret>,
}

/// Prometheus metrics export (opt-in). The daemon's reason to exist: sample →
/// SQLite → expose. Two independent paths, configured here and via the wizard.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    #[serde(default)]
    pub pull: PullConfig,
    #[serde(default)]
    pub push: PushConfig,
}

/// Prometheus pull: a tiny HTTP `/metrics` endpoint scrapers hit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bind address. Defaults to loopback — SECURITY: never bind a wider
    /// interface without intent. Always `127.0.0.1`, never `localhost`.
    #[serde(default = "defaults::metrics_addr")]
    pub addr: String,
}

impl Default for PullConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: defaults::metrics_addr(),
        }
    }
}

/// Push: periodically POST the metrics as JSON to an ingestion endpoint (e.g.
/// OpenObserve's `_json` ingest). Off unless explicitly enabled.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Full ingestion URL, e.g. `https://oo.example/api/default/ghr/_json`.
    #[serde(default)]
    pub endpoint: String,
    /// Optional `Authorization` header value (e.g. "Basic …"). Never logged.
    #[serde(default)]
    pub auth: Option<Secret>,
    #[serde(default = "defaults::push_interval")]
    pub interval_secs: u64,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            auth: None,
            interval_secs: defaults::push_interval(),
        }
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        match crate::paths::resolve_config(explicit) {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| Error::Config(format!("reading {}: {e}", p.display())))?;
                toml::from_str(&text)
                    .map_err(|e| Error::Config(format!("parsing {}: {e}", p.display())))
            }
            None => Ok(Config::default()),
        }
    }

    /// Resolve the read-only PAT to use for `org`.
    /// Precedence: per-org config token → `GHR_STATS_GITHUB_TOKEN` env →
    /// single fallback config token.
    pub fn github_token_for(&self, org: &str) -> Option<String> {
        if let Some(t) = self.github.tokens.get(org) {
            return Some(t.expose().to_string());
        }
        if let Ok(t) = std::env::var("GHR_STATS_GITHUB_TOKEN")
            && !t.is_empty()
        {
            return Some(t);
        }
        self.github.token.as_ref().map(|s| s.expose().to_string())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: defaults::db_path(),
            event_log: defaults::event_log(),
            runner_roots: defaults::runner_roots(),
            orgs: Vec::new(),
            intervals: Intervals::default(),
            github: GithubConfig::default(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            local_secs: defaults::local_secs(),
            api_secs: defaults::api_secs(),
        }
    }
}

/// Field defaults. Path defaults delegate to [`crate::paths`] so the privilege
/// scope (euid) decides system vs user locations in exactly one place.
mod defaults {
    use std::path::PathBuf;

    use crate::paths::Scope;

    pub fn db_path() -> PathBuf {
        Scope::detect().db_path()
    }

    pub fn event_log() -> PathBuf {
        Scope::detect().event_log()
    }

    pub fn runner_roots() -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn local_secs() -> u64 {
        5
    }

    pub fn api_secs() -> u64 {
        60
    }

    pub fn metrics_addr() -> String {
        "127.0.0.1:9477".to_string()
    }

    pub fn push_interval() -> u64 {
        30
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_populated() {
        let c = Config::default();
        assert_eq!(c.intervals.local_secs, 5);
        assert_eq!(c.intervals.api_secs, 60);
        // Generalized for distribution: no host-specific runner root is assumed.
        assert!(c.runner_roots.is_empty());
        assert!(c.orgs.is_empty());
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.db_path, defaults::db_path());
        assert_eq!(c.intervals.api_secs, 60);
    }

    #[test]
    fn per_org_token_takes_precedence() {
        let c: Config =
            toml::from_str("[github.tokens]\n\"example-org\" = \"github_pat_xyz\"\n").unwrap();
        // Per-org token resolves before the env/fallback path, so this is
        // deterministic regardless of GHR_STATS_GITHUB_TOKEN in the test env.
        assert_eq!(
            c.github_token_for("example-org").as_deref(),
            Some("github_pat_xyz")
        );
    }
}
