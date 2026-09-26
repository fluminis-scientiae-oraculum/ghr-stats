//! Config schema and defaults; every field has a default. Locations: [`crate::shared::paths`].

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
    #[serde(default = "defaults::db_path")]
    pub db_path: PathBuf,

    /// Roots scanned for runner install dirs (each holds a `.runner` file).
    #[serde(default = "defaults::runner_roots")]
    pub runner_roots: Vec<PathBuf>,

    /// Empty ⇒ derived from the orgs in `.runner` files.
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
    /// Local sampling cadence (runners, processes, host).
    #[serde(default = "defaults::local_secs")]
    pub local_secs: u64,
    #[serde(default = "defaults::api_secs")]
    pub api_secs: u64,
    /// Unset ⇒ derived from `api_secs`; see [`Intervals::api_max_age`].
    #[serde(default)]
    pub api_max_age_secs: Option<u64>,
}

impl Intervals {
    /// Max age of a GitHub reconcile row still served as current: the override, else
    /// three polls (so one slow cycle doesn't flap everything stale), floored at 180 s.
    pub fn api_max_age(&self) -> u64 {
        self.api_max_age_secs
            .unwrap_or_else(|| self.api_secs.saturating_mul(3).max(180))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// Fallback for orgs without a per-org token; `GHR_STATS_GITHUB_TOKEN` overrides it.
    #[serde(default)]
    pub token: Option<Secret>,
    /// Org login → read-only PAT.
    #[serde(default)]
    pub tokens: BTreeMap<String, Secret>,
}

/// Prometheus metrics export (opt-in).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    #[serde(default)]
    pub pull: PullConfig,
    #[serde(default)]
    pub push: PushConfig,
}

/// HTTP `/metrics` endpoint for scrapers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullConfig {
    #[serde(default)]
    pub enabled: bool,
    /// SECURITY: loopback by default; never bind wider without intent.
    /// Always `127.0.0.1`, never `localhost`.
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

/// Periodically POST metrics as JSON to an ingest endpoint (e.g. OpenObserve `_json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub endpoint: String,
    /// `Authorization` header value. Never logged.
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
                // Non-root can't read the 0600 root-owned /etc config; defaults let
                // the TUI still launch and read data over the socket.
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

/// Hot-swappable config shared across collector threads. Lock poisoning is
/// recovered so a panicking holder can't wedge the daemon.
#[derive(Clone)]
pub struct SharedConfig(Arc<RwLock<Arc<Config>>>);

impl SharedConfig {
    pub fn new(cfg: Config) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(cfg))))
    }

    pub fn snapshot(&self) -> Arc<Config> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn store(&self, cfg: Config) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
    }
}

/// Lenient view of a config's PATs: ignores every other field, so it survives schema drift.
#[derive(Deserialize, Default)]
struct TokenPeek {
    #[serde(default)]
    github: GithubPeek,
}

#[derive(Deserialize, Default)]
struct GithubPeek {
    #[serde(default)]
    tokens: BTreeMap<String, toml::Value>,
    #[serde(default)]
    token: Option<toml::Value>,
}

pub(crate) fn count_tokens(config_text: &str) -> Option<usize> {
    let peek: TokenPeek = toml::from_str(config_text).ok()?;
    Some(peek.github.tokens.len() + usize::from(peek.github.token.is_some()))
}

pub(crate) fn token_orgs(config_text: &str) -> Vec<String> {
    toml::from_str::<TokenPeek>(config_text)
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
            api_max_age_secs: None,
        }
    }
}

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
    fn api_max_age_tracks_api_secs_unless_overridden() {
        let mut i = Intervals::default();
        assert_eq!(i.api_max_age(), 180);
        i.api_secs = 300;
        assert_eq!(i.api_max_age(), 900);
        i.api_secs = 10;
        assert_eq!(i.api_max_age(), 180);
        i.api_max_age_secs = Some(45);
        assert_eq!(i.api_max_age(), 45);
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
        let one = "[github]\ntoken = \"github_pat_x\"\n";
        assert_eq!(count_tokens(one), Some(1));
        assert_eq!(count_tokens("runner_roots = []\n"), Some(0));
        assert_eq!(count_tokens("this is not = = toml ["), None);
    }

    #[test]
    fn token_orgs_returns_sorted_keys_without_exposing_values() {
        let cfg = "runner_roots = []\n\n[github.tokens]\nwidgets = \"github_pat_SECRET\"\nacme = \"github_pat_OTHER\"\n";
        assert_eq!(token_orgs(cfg), vec!["acme", "widgets"]);
        assert!(token_orgs("[github]\ntoken = \"github_pat_x\"\n").is_empty());
        assert!(token_orgs("runner_roots = []\n").is_empty());
        assert!(token_orgs("this is not = = toml [").is_empty());
    }

    #[test]
    fn per_org_token_takes_precedence() {
        let c: Config =
            toml::from_str("[github.tokens]\n\"example-org\" = \"github_pat_xyz\"\n").unwrap();
        // Per-org wins before the env var is consulted, so the test env can't interfere.
        assert_eq!(
            c.github_token_for("example-org").as_deref(),
            Some("github_pat_xyz")
        );
    }
}
