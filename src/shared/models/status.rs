//! What we answer with: the verdict and the machine-facing payload.

use serde::{Deserialize, Serialize};

use super::Liveness;

/// Overall health call; also the process exit code, so `$?` matches the printed verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ok,
    Degraded,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Degraded => "degraded",
            Verdict::Unknown => "unknown",
        }
    }

    /// Exit code `3` (usage error) is raised by the CLI before any verdict exists.
    pub fn exit_code(self) -> u8 {
        match self {
            Verdict::Ok => 0,
            Verdict::Degraded => 1,
            Verdict::Unknown => 2,
        }
    }
}

impl From<Verdict> for std::process::ExitCode {
    fn from(v: Verdict) -> Self {
        std::process::ExitCode::from(v.exit_code())
    }
}

/// Which data plane an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No collector: a live local scan only, so nothing GitHub-side is knowable.
    Ephemeral,
    Persistent,
}

impl Mode {
    /// Must match the serde spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ephemeral => "ephemeral",
            Mode::Persistent => "persistent",
        }
    }
}

/// Payload of `ghr-stats status --json` and `Query::FleetStatus`. Fields are machine-stable;
/// bump `schema_version` on any breaking change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetStatus {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub mode: Mode,
    pub verdict: Verdict,
    pub fleet: FleetCounts,
    pub orgs: Vec<OrgStatus>,
    pub runners: Vec<RunnerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetCounts {
    pub runners: u32,
    pub busy: u32,
    pub idle: u32,
    pub offline: u32,
    /// Cross-cuts busy/idle/offline rather than partitioning them.
    pub divergent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgStatus {
    pub org: String,
    pub runners: u32,
    pub github_online: u32,
    /// Seconds since the last successful reconcile; `None` if never, or in Ephemeral mode.
    pub reconcile_age_s: Option<i64>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerStatus {
    pub name: String,
    pub org: String,
    pub agent_id: i64,
    pub liveness: Liveness,
    pub state_seconds: i64,
    /// `None` without a fresh GitHub reading.
    pub github_online: Option<bool>,
    pub github_busy: Option<bool>,
    pub github_offline_seconds: Option<i64>,
    pub github_sample_age_s: Option<i64>,
    pub divergent: Option<bool>,
    pub cpu_percent: Option<f32>,
    pub mem_bytes: Option<u64>,
}
