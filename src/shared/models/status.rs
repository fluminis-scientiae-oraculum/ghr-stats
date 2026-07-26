//! What we ANSWER with: the verdict, and the payload that carries it.
//!
//! Everything else in [`super`] records a fact. These types make a CALL about
//! those facts, which is why they are together and why they are apart from the
//! rest — the same RETRIEVE-vs-JUDGE line drawn in `metrics::encode`.
//!
//! [`Verdict`] owns both the serialized string and the process exit code, from
//! ONE enum, so the printed verdict and `$?` cannot drift apart. The exit code is
//! part of the interface: it lets an agent branch without parsing.
//!
//! [`Mode`] lives here rather than in the TUI that first needed it because it is
//! now part of a machine-facing payload — it says which data plane an answer came
//! from, which a caller must know to interpret a `null`.

use serde::{Deserialize, Serialize};

use super::Liveness;

/// The overall health call, and the process exit code, from ONE enum.
///
/// The exit code is part of the interface — it lets an agent branch without
/// parsing — so it must never disagree with the `verdict` field it ships
/// alongside. Deriving both here makes that disagreement unrepresentable rather
/// than a doc note someone has to honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every runner healthy, and GitHub agrees.
    Ok,
    /// At least one runner offline, divergent, or stale beyond the window.
    Degraded,
    /// No collector AND no readable runner root — we cannot say anything.
    Unknown,
}

impl Verdict {
    /// The documented exit code. `3` (usage/config error) is not reachable from
    /// a verdict — it is a CLI-argument failure, raised before any status is
    /// computed.
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
///
/// Lives here, not in the TUI that first needed it, because it is now part of a
/// machine-facing payload: `mode` was a `String` on [`FleetStatus`], so every
/// consumer that wanted to branch on it — the dashboard badge, `status`,
/// `explain` — had to compare a magic literal, and a rename in one place would
/// have silently changed nobody's mind but its own. One enum, one spelling, and
/// the serialized form is still exactly `"ephemeral"` / `"persistent"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No collector: a live local scan only, so nothing GitHub-side is knowable.
    Ephemeral,
    /// The collector answered, so history and the GitHub view are available.
    Persistent,
}

impl Mode {
    /// The wire spelling, for plain-text renderings that must match the JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Ephemeral => "ephemeral",
            Mode::Persistent => "persistent",
        }
    }
}

/// Machine-facing fleet snapshot — the payload of `ghr-stats status --json` and
/// of `Query::FleetStatus`.
///
/// Every field is machine-stable: no ANSI, no thousands separators, no localised
/// time, both ISO-8601 and epoch. `schema_version` is bumped on any breaking
/// change so a consumer can refuse a payload it does not understand.
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
    /// Seconds since this org's last SUCCESSFUL reconcile. `None` when it has
    /// never succeeded, or in Ephemeral mode.
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
    /// `null` — never invented — when there is no current GitHub reading.
    pub github_online: Option<bool>,
    pub github_busy: Option<bool>,
    pub github_offline_seconds: Option<i64>,
    pub github_sample_age_s: Option<i64>,
    pub divergent: Option<bool>,
    pub cpu_percent: Option<f32>,
    pub mem_bytes: Option<u64>,
}
