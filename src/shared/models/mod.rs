//! Domain types shared across collectors, store, and the TUI.
//!
//! Runner identity comes from each runner's own `.runner` config file
//! (authoritative) plus the owning OS user of its install directory — never
//! from parsing systemd unit names. The numeric `agent_id` is the stable join
//! key to the GitHub API.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Static identity of a self-hosted runner, read from its `.runner` file plus
/// the owning OS user of its install directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// GitHub runner id (`agentId` in `.runner`) — the join key to the API.
    pub agent_id: i64,
    /// Runner display name (`agentName`), e.g. "runner-01".
    pub name: String,
    /// Owning GitHub org, derived from `.runner`'s `gitHubUrl`.
    pub org: String,
    /// Runner group (`poolName`), e.g. "Default Group".
    pub group: Option<String>,
    /// Install directory, e.g. /srv/actions-runner/runner-01.
    pub dir: PathBuf,
    /// Work folder name (`workFolder`), e.g. "_work".
    pub work_folder: String,
    /// Owning uid of the install dir — the authoritative handle for matching
    /// the runner's processes (`/proc/<pid>` owner) and cgroup.
    pub uid: u32,
    /// Owning linux user name, resolved from `uid` for display (e.g.
    /// "runner-01"). Falls back to the uid as a string if unresolvable.
    pub user: String,
}

/// systemd-free liveness, derived from the runner user's processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// Listener process present, no job worker.
    Idle,
    /// A job worker process is running.
    Busy,
    /// No listener process found.
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

    /// Parse the stored `liveness` text; an unknown value fails safe to
    /// `Offline` (a corrupt row never crashes a read).
    pub fn from_db(s: &str) -> Liveness {
        match s {
            "busy" => Liveness::Busy,
            "idle" => Liveness::Idle,
            _ => Liveness::Offline,
        }
    }
}

/// A point-in-time sample of one runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSample {
    pub ts: i64,
    pub agent_id: i64,
    pub name: String,
    pub org: String,
    pub liveness: Liveness,
    pub current_run_id: Option<i64>,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub uptime_s: Option<u64>,
}

/// Current per-runner liveness plus the timestamp of the last liveness *change*
/// (the "edge"). One row per runner, upserted by the writer; survives restarts,
/// so "Idle/Active for <dur>" = `now - since_ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerState {
    pub agent_id: i64,
    pub liveness: Liveness,
    pub since_ts: i64,
    pub last_seen_ts: i64,
}

/// Per-NUMA-node memory, read from /sys/devices/system/node/node*/meminfo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumaNode {
    pub node: u32,
    pub mem_total: u64,
    pub mem_free: u64,
}

/// Host-wide resource snapshot.
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

/// A runner's state as GitHub reports it (from the API reconcile pass).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRunnerRow {
    pub agent_id: i64,
    pub org: String,
    pub name: String,
    pub online: bool,
    pub busy: bool,
}

// --- read/query projections ---
//
// The shapes the store's read queries return. They double as the IPC wire
// payloads (the collector serves them; the TUI renders them), so they live here
// in the shared domain rather than inside the service's store — that is what
// lets the TUI depend on these types without depending on `service::store`.

/// A recent job, joined from hook timing + (eventually) API conclusion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRow {
    pub runner_name: String,
    pub repo: String,
    pub job: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub conclusion: Option<String>,
}

/// A completed `job_event` whose pass/fail conclusion has not yet been resolved
/// from the GitHub API (the reconcile's work-list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConclusion {
    pub org: String,
    pub repo: String,
    pub run_id: i64,
    pub run_attempt: i64,
    pub job: String,
    pub runner_name: String,
}

/// A resolved job conclusion to write back to `job_event`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobConclusion {
    pub run_id: i64,
    pub run_attempt: i64,
    pub job: String,
    pub runner_name: String,
    pub conclusion: String,
}

/// GitHub's view of one runner (from the latest reconcile tick).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ApiState {
    pub online: bool,
    pub busy: bool,
}

/// One historical runner sample, for sparklines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistPoint {
    pub ts: i64,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
}

/// One host time-series point.
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

/// One fleet-occupancy point: how many runners were busy / online at a tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusyPoint {
    pub ts: i64,
    pub busy: u32,
    pub online: u32,
}
