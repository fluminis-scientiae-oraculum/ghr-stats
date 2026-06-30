//! Domain types shared across collectors, store, and the TUI.
//!
//! Runner identity comes from each runner's own `.runner` config file
//! (authoritative) plus the owning OS user of its install directory — never
//! from parsing systemd unit names. The numeric `agent_id` is the stable join
//! key to the GitHub API.
#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Static identity of a self-hosted runner, read from its `.runner` file plus
/// the owning OS user of its install directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// GitHub runner id (`agentId` in `.runner`) — the join key to the API.
    pub agent_id: i64,
    /// Runner display name (`agentName`), e.g. "fso-epoch-immer-00".
    pub name: String,
    /// Owning GitHub org, derived from `.runner`'s `gitHubUrl`.
    pub org: String,
    /// Runner group (`poolName`), e.g. "FSO Owned".
    pub group: Option<String>,
    /// Install directory, e.g. /mnt/store/ghr/ghr_immer_00.
    pub dir: PathBuf,
    /// Work folder name (`workFolder`), e.g. "_work".
    pub work_folder: String,
    /// Owning uid of the install dir — the authoritative handle for matching
    /// the runner's processes (`/proc/<pid>` owner) and cgroup.
    pub uid: u32,
    /// Owning linux user name, resolved from `uid` for display (e.g.
    /// "ghr_immer_00"). Falls back to the uid as a string if unresolvable.
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

/// A job lifecycle event: timing from local hooks, outcome from the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEvent {
    pub run_id: i64,
    pub run_attempt: i64,
    pub job: String,
    pub repo: String,
    pub org: String,
    pub runner_name: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub conclusion: Option<String>,
    /// "hook" | "api".
    pub source: String,
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

/// Org queue-depth sample (from the API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSample {
    pub ts: i64,
    pub org: String,
    pub queued: u32,
    pub in_progress: u32,
}
