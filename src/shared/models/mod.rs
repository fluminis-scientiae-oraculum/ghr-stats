//! Domain types shared by collectors, store, IPC and TUI; submodules split by where the
//! fact came from. Runner identity: the install `dir` is the local key; `agent_id` is
//! unique only per org, so it joins GitHub as `(org, agent_id)`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod timeline;

mod github;
mod jobs;
mod status;

pub use github::{
    ApiErrorKind, ApiOrgOutcome, ApiReconcileState, ApiRunnerRow, ApiRunnerState, ApiState,
    GhCount, GhView,
};
pub use jobs::{JobConclusion, JobRow, PendingConclusion};
pub use status::{FleetCounts, FleetStatus, Mode, OrgStatus, RunnerStatus, Verdict};

/// Static identity of a runner from its `.runner` file (`agentId`, `agentName`,
/// `gitHubUrl` → org, `poolName`, `workFolder`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInfo {
    pub agent_id: i64,
    pub name: String,
    /// The scope's login: org, repository owner or enterprise.
    pub org: String,
    pub scope: crate::shared::github::RunnerScope,
    pub group: Option<String>,
    pub dir: PathBuf,
    pub work_folder: String,
    /// Install dir owner; the uid when it has no name.
    pub user: String,
}

/// From the runner user's processes: listener only ⇒ Idle, a job worker ⇒ Busy, no listener
/// ⇒ Offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Idle,
    Busy,
    Offline,
}

impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Liveness::Idle => "idle",
            Liveness::Busy => "busy",
            Liveness::Offline => "offline",
        }
    }
}

/// An unknown stored label is an error, never a guessed state.
impl rusqlite::types::FromSql for Liveness {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match v.as_str()? {
            "busy" => Ok(Liveness::Busy),
            "idle" => Ok(Liveness::Idle),
            "offline" => Ok(Liveness::Offline),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown liveness {other:?}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSample {
    pub ts: i64,
    pub agent_id: i64,
    pub dir: String,
    pub name: String,
    pub org: String,
    pub liveness: Liveness,
    pub cpu_pct: Option<f32>,
    /// Working-set memory (anon + shmem).
    pub mem_bytes: Option<u64>,
    /// Raw cgroup `memory.current` (working set + reclaimable page cache).
    pub mem_current_bytes: Option<u64>,
    pub uptime_s: Option<u64>,
}

/// Current liveness and when it last changed (`since_ts`); persisted, so durations survive
/// restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerState {
    pub dir: String,
    pub liveness: Liveness,
    pub since_ts: i64,
    pub last_seen_ts: i64,
}

/// From `/sys/devices/system/node/node*/meminfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumaNode {
    pub node: u32,
    pub mem_total: u64,
    pub mem_free: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostSample {
    pub ts: i64,
    pub load1: f64,
    pub load5: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub numa: Vec<NumaNode>,
    /// Total bytes across all runners' `_work` dirs (slow cadence).
    pub work_bytes: Option<u64>,
    /// Bytes used on /tmp.
    pub tmp_bytes: Option<u64>,
    /// Free bytes on the filesystem holding the runner roots.
    pub root_free: Option<u64>,
}

/// Locally healthy, but GitHub says the runner can't take work; `None` unless the GitHub
/// view is fresh. Not a [`Liveness`] variant: that stays a purely local fact.
pub fn divergent(liveness: Liveness, gh: GhView) -> Option<bool> {
    match (liveness, gh) {
        // Locally down already shows in every other signal; don't double-count it.
        (Liveness::Offline, _) => Some(false),
        (_, GhView::Fresh { state, .. }) => Some(!state.online),
        (_, GhView::Stale { .. } | GhView::Unknown) => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistPoint {
    pub ts: i64,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostPoint {
    pub ts: i64,
    pub load1: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub tmp_bytes: Option<u64>,
    pub work_bytes: Option<u64>,
    pub root_free: Option<u64>,
}

/// Fleet occupancy at one tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusyPoint {
    pub ts: i64,
    pub busy: u32,
    /// Locally online (listener present).
    pub online: u32,
    /// `None` when no runner had a fresh GitHub reading: plot a gap, not zero.
    pub github: Option<GhCount>,
}
