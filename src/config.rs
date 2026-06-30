#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// Runtime configuration. Loaded from a TOML file (see `resolve_path` for the
/// search order); every field has a default, so the tool runs with no config.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// SQLite database path.
    #[serde(default = "defaults::db_path")]
    pub db_path: PathBuf,

    /// Append-only NDJSON job-event log written by the runner hooks (P4).
    #[serde(default = "defaults::event_log")]
    pub event_log: PathBuf,

    /// Roots scanned for runner install dirs (each contains a `.runner` file).
    #[serde(default = "defaults::runner_roots")]
    pub runner_roots: Vec<PathBuf>,

    /// GitHub orgs to poll for queue depth / runner reconcile (P3+).
    /// Empty ⇒ derived from the orgs discovered in `.runner` files.
    #[serde(default)]
    pub orgs: Vec<String>,

    #[serde(default)]
    pub intervals: Intervals,

    #[serde(default)]
    pub github: GithubConfig,
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
    /// Fallback token for any org without a specific one below. A fine-grained
    /// PAT is scoped to ONE org, so prefer per-org `tokens`; this suits a
    /// single-org setup or a classic multi-org PAT. `GHR_STATS_GITHUB_TOKEN`
    /// (env) overrides this fallback.
    #[serde(default)]
    pub token: Option<Secret>,
    /// Per-org fine-grained read-only PATs: org login → token.
    #[serde(default)]
    pub tokens: BTreeMap<String, Secret>,
}

/// A string that never reveals itself in `Debug` output (e.g. API tokens).
#[derive(Clone, Deserialize)]
pub struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\"***\")")
    }
}

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        match Self::resolve_path(explicit) {
            Some(p) => {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| Error::Config(format!("reading {}: {e}", p.display())))?;
                toml::from_str(&text)
                    .map_err(|e| Error::Config(format!("parsing {}: {e}", p.display())))
            }
            None => Ok(Config::default()),
        }
    }

    /// Search order: `--config` flag, `$GHR_STATS_CONFIG`,
    /// `/etc/ghr-stats/config.toml`, then `$XDG_CONFIG_HOME/ghr-stats/config.toml`.
    fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.to_path_buf());
        }
        if let Some(p) = std::env::var_os("GHR_STATS_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let etc = PathBuf::from("/etc/ghr-stats/config.toml");
        if etc.exists() {
            return Some(etc);
        }
        let xdg = xdg_config_dir().join("ghr-stats/config.toml");
        if xdg.exists() {
            return Some(xdg);
        }
        None
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

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn xdg_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}

mod defaults {
    use std::path::PathBuf;

    fn data_dir() -> PathBuf {
        super::xdg_data_dir().join("ghr-stats")
    }

    pub fn db_path() -> PathBuf {
        data_dir().join("ghr-stats.db")
    }

    pub fn event_log() -> PathBuf {
        data_dir().join("events.ndjson")
    }

    pub fn runner_roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/mnt/store/ghr")]
    }

    pub fn local_secs() -> u64 {
        5
    }

    pub fn api_secs() -> u64 {
        60
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
        assert_eq!(c.runner_roots, vec![PathBuf::from("/mnt/store/ghr")]);
        assert!(c.orgs.is_empty());
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.db_path, defaults::db_path());
        assert_eq!(c.intervals.api_secs, 60);
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        let s = Secret("super-secret-token".to_string());
        let rendered = format!("{s:?}");
        assert_eq!(rendered, "Secret(\"***\")");
        assert!(!rendered.contains("super-secret-token"));
    }
}
