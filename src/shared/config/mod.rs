//! Config schema and defaults; every field has a default. Locations: [`crate::shared::paths`].

pub(crate) mod persist;
mod secret;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;

pub use secret::Secret;

use crate::shared::error::{Error, Result};
use crate::shared::github::{GitHubHost, TokenKey};

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

    /// Days of samples kept (default 30), or `"forever"`. Job history is always kept.
    #[serde(default)]
    pub retention_days: Retention,

    #[serde(default)]
    pub intervals: Intervals,

    #[serde(default)]
    pub github: GithubConfig,

    #[serde(default)]
    pub metrics: MetricsConfig,

    #[serde(skip)]
    pub provenance: Provenance,
}

/// Where a loaded config's values came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Provenance {
    File(PathBuf),
    /// No config file exists; every value is a default.
    #[default]
    Absent,
    /// The file exists but this user cannot read it; every value is a default.
    Unreadable(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RetentionSetting")]
pub enum Retention {
    Days(NonZeroU16),
    Forever,
}

impl Default for Retention {
    fn default() -> Self {
        Retention::Days(NonZeroU16::new(30).expect("30 is non-zero"))
    }
}

impl Retention {
    /// Samples older than this epoch second are pruned; `None` keeps everything.
    pub fn cutoff(self, now: i64) -> Option<i64> {
        match self {
            Retention::Days(d) => Some(now - i64::from(d.get()) * 86_400),
            Retention::Forever => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RetentionSetting {
    Days(u16),
    Word(String),
}

impl TryFrom<RetentionSetting> for Retention {
    type Error = String;

    fn try_from(s: RetentionSetting) -> std::result::Result<Self, String> {
        match s {
            RetentionSetting::Days(d) => NonZeroU16::new(d)
                .map(Retention::Days)
                .ok_or_else(|| "retention_days must be at least 1, or \"forever\"".to_string()),
            RetentionSetting::Word(w) if w == "forever" => Ok(Retention::Forever),
            RetentionSetting::Word(w) => Err(format!(
                "retention_days is a number of days or \"forever\", not {w:?}"
            )),
        }
    }
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
    /// Read-only PAT per `owner` (github.com) or `host/owner`.
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
    pub addr: SocketAddr,
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
        let Some(p) = crate::shared::paths::resolve_config(explicit) else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&p) {
            Ok(text) => {
                let mut cfg: Config = toml::from_str(&text)
                    .map_err(|e| Error::Config(format!("parsing {}: {e}", p.display())))?;
                cfg.provenance = Provenance::File(p);
                Ok(cfg)
            }
            // Non-root can't read the 0600 root-owned /etc config; defaults let the TUI
            // and read verbs still run against the collector.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Ok(Config {
                provenance: Provenance::Unreadable(p),
                ..Config::default()
            }),
            Err(e) => Err(Error::Config(format!("reading {}: {e}", p.display()))),
        }
    }

    /// An error naming the unreadable file, for operations that must not act on defaults.
    pub fn require_readable(&self) -> Result<()> {
        match &self.provenance {
            Provenance::Unreadable(p) => Err(Error::Config(format!(
                "{} is not readable by this user, so its settings are unknown — run as root, \
                 or point --config or GHR_STATS_CONFIG at a file this user can read",
                p.display()
            ))),
            Provenance::File(_) | Provenance::Absent => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn dotcom_token(&self, owner: &str) -> Option<String> {
        self.github_token_for(&GitHubHost::dotcom(), owner)
            .map(|t| t.expose().to_string())
    }

    /// The PAT for `owner` on `host`: a `[github.tokens]` entry keyed `owner` (github.com)
    /// or `host/owner`, else — for github.com only — `GHR_STATS_GITHUB_TOKEN` or
    /// `github.token`. A github.com token is never sent to another host.
    pub fn github_token_for(&self, host: &GitHubHost, owner: &str) -> Option<Secret> {
        let keyed = self
            .github
            .tokens
            .iter()
            .find(|(key, _)| TokenKey::parse(key).is_ok_and(|k| k.matches(host, owner)));
        if let Some((_, t)) = keyed {
            return Some(t.clone());
        }
        if !host.is_dotcom() {
            return None;
        }
        std::env::var("GHR_STATS_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .map(Secret::from)
            .or_else(|| self.github.token.clone())
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
            retention_days: Retention::default(),
            provenance: Provenance::Absent,
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

    pub fn metrics_addr() -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], 9477))
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
    fn tokens_are_scoped_to_their_host() {
        let c: Config = toml::from_str(
            "[github]\ntoken = \"github_pat_fallback\"\n\
             [github.tokens]\n\"Example-Org\" = \"github_pat_dotcom\"\n\
             \"ghe.example.com/eng\" = \"github_pat_ghes\"\n",
        )
        .unwrap();
        let ghes = GitHubHost::parse("ghe.example.com").unwrap();
        let expose = |t: Option<Secret>| t.map(|t| t.expose().to_string());
        assert_eq!(
            expose(c.github_token_for(&GitHubHost::dotcom(), "example-org")).as_deref(),
            Some("github_pat_dotcom")
        );
        assert_eq!(
            expose(c.github_token_for(&ghes, "eng")).as_deref(),
            Some("github_pat_ghes")
        );
        assert_eq!(expose(c.github_token_for(&ghes, "example-org")), None);
    }

    #[test]
    fn retention_is_days_or_forever() {
        let parse = |t: &str| toml::from_str::<Config>(t).map(|c| c.retention_days);
        assert_eq!(parse("").unwrap(), Retention::default());
        assert_eq!(
            parse("retention_days = \"forever\"").unwrap(),
            Retention::Forever
        );
        assert_eq!(
            parse("retention_days = 7").unwrap().cutoff(1_000_000),
            Some(395_200)
        );
        assert!(parse("retention_days = 0").is_err());
        assert!(parse("retention_days = \"always\"").is_err());
    }

    #[test]
    fn per_org_token_takes_precedence() {
        let c: Config =
            toml::from_str("[github.tokens]\n\"example-org\" = \"github_pat_xyz\"\n").unwrap();
        // Per-org wins before the env var is consulted, so the test env can't interfere.
        assert_eq!(
            c.dotcom_token("example-org").as_deref(),
            Some("github_pat_xyz")
        );
    }
}
