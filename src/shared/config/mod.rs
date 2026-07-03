//! Runtime configuration loaded from a TOML file.
//!
//! *Where* the file lives and *where* data is written is decided by
//! [`crate::shared::paths`] (privilege-scoped). This module owns only the config
//! schema, its defaults, and read-only token resolution. Every field has a
//! default, so the tool runs with no config at all.

pub(crate) mod persist;
mod secret;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;

pub use secret::Secret;

use crate::shared::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// SQLite database path.
    #[serde(default = "defaults::db_path")]
    pub db_path: PathBuf,

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

/// Prometheus metrics export (opt-in) — one of the collector's outputs: sample →
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
        match crate::shared::paths::resolve_config(explicit) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(text) => toml::from_str(&text)
                    .map_err(|e| Error::Config(format!("parsing {}: {e}", p.display()))),
                // A non-root process can't read the root-owned system config
                // (0600 at /etc). That's expected in a system deployment — run
                // `sudo ghr-stats` for local config. Fall back to defaults so the
                // dashboard still launches and reads persistent data over the
                // socket, rather than refusing to start.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(
                        path = %p.display(),
                        "config not readable without root — using defaults (run `sudo ghr-stats` for local config)"
                    );
                    Ok(Config::default())
                }
                Err(e) => Err(Error::Config(format!("reading {}: {e}", p.display()))),
            },
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

/// A hot-swappable config snapshot shared across the collector's threads. The
/// collector loads config once, then swaps in a fresh snapshot when a mutation
/// (or any reload) changes it; every worker that reads its snapshot each cycle
/// picks the change up live. Zero-dep (`RwLock<Arc<Config>>`): a read clones the
/// inner `Arc` (not the `Config`) and releases the lock immediately, so the
/// rare writer never blocks the read-mostly workers. Lock poisoning is recovered
/// (a panic while holding this brief lock must not wedge the daemon).
#[derive(Clone)]
pub struct SharedConfig(Arc<RwLock<Arc<Config>>>);

impl SharedConfig {
    pub fn new(cfg: Config) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(cfg))))
    }

    /// A cheap snapshot of the current config (an `Arc` clone, not a deep copy).
    pub fn snapshot(&self) -> Arc<Config> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Swap in a new config; readers observe it on their next [`Self::snapshot`].
    pub fn store(&self, cfg: Config) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
    }
}

/// Count the read-only PATs in a config file's TEXT without exposing any value —
/// a lenient peek that ignores every other field (so it survives schema drift).
/// `None` if the text doesn't parse. Shared by the teardown plan's redacted
/// preview and the TUI's host-inventory line. Pure.
pub(crate) fn count_tokens(config_text: &str) -> Option<usize> {
    #[derive(Deserialize, Default)]
    struct Peek {
        #[serde(default)]
        github: Gh,
    }
    #[derive(Deserialize, Default)]
    struct Gh {
        #[serde(default)]
        tokens: BTreeMap<String, toml::Value>,
        #[serde(default)]
        token: Option<toml::Value>,
    }
    let peek: Peek = toml::from_str(config_text).ok()?;
    Some(peek.github.tokens.len() + usize::from(peek.github.token.is_some()))
}

/// The org logins that have a configured per-org read-only PAT — presence only,
/// no token value — parsed leniently from a config file's TEXT (ignoring every
/// other field, so it survives schema drift). Empty if the text doesn't parse.
/// Lets the root collector report which orgs are configured to a non-root TUI
/// over the IPC socket without ever exposing a secret. Sorted (BTreeMap). Pure.
pub(crate) fn token_orgs(config_text: &str) -> Vec<String> {
    #[derive(Deserialize, Default)]
    struct Peek {
        #[serde(default)]
        github: Gh,
    }
    #[derive(Deserialize, Default)]
    struct Gh {
        #[serde(default)]
        tokens: BTreeMap<String, toml::Value>,
    }
    toml::from_str::<Peek>(config_text)
        .map(|p| p.github.tokens.into_keys().collect())
        .unwrap_or_default()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: defaults::db_path(),
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

/// Field defaults. Path defaults delegate to [`crate::shared::paths`] so the privilege
/// scope (euid) decides system vs user locations in exactly one place.
mod defaults {
    use std::path::PathBuf;

    use crate::shared::paths::Scope;

    pub fn db_path() -> PathBuf {
        Scope::detect().db_path()
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
    fn count_tokens_counts_without_exposing_values() {
        let cfg = "runner_roots = []\n\n[github.tokens]\nacme = \"github_pat_SECRET_VALUE\"\nwidgets = \"github_pat_OTHER\"\n";
        assert_eq!(count_tokens(cfg), Some(2));
        // A fallback token counts too.
        let one = "[github]\ntoken = \"github_pat_x\"\n";
        assert_eq!(count_tokens(one), Some(1));
        // No tokens.
        assert_eq!(count_tokens("runner_roots = []\n"), Some(0));
        // Malformed ⇒ None (the caller still shows the file, just can't count).
        assert_eq!(count_tokens("this is not = = toml ["), None);
    }

    #[test]
    fn token_orgs_returns_sorted_keys_without_exposing_values() {
        let cfg = "runner_roots = []\n\n[github.tokens]\nwidgets = \"github_pat_SECRET\"\nacme = \"github_pat_OTHER\"\n";
        // Sorted (BTreeMap), and never the token values.
        assert_eq!(token_orgs(cfg), vec!["acme", "widgets"]);
        // Fallback-only / no per-org tokens ⇒ empty (matches the per-org display).
        assert!(token_orgs("[github]\ntoken = \"github_pat_x\"\n").is_empty());
        assert!(token_orgs("runner_roots = []\n").is_empty());
        // Malformed ⇒ empty (best-effort peek, never panics).
        assert!(token_orgs("this is not = = toml [").is_empty());
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
