//! Domain types shared across collectors, store, and the TUI.
//!
//! Runner identity comes from each runner's own `.runner` config file
//! (authoritative) plus the owning OS user of its install directory — never
//! from parsing systemd unit names. Two identities matter and must not be
//! confused: the install `dir` is the LOCALLY-unique key (one per runner on a
//! host), while the numeric `agent_id` is GitHub's runner id — unique only
//! *within* an org, so it joins to the API as `(org, agent_id)`. Keying local
//! state (CPU rate, liveness edge) by `agent_id` alone conflates two runners in
//! different orgs that were assigned the same id; key those by `dir`.
//!
//! Cut by WHERE THE FACT CAME FROM:
//!
//! - **this file** — what this host measured for itself: runner identity, the
//!   local liveness edge, the host and occupancy series.
//! - [`github`] — what GitHub said, and how well we could ask.
//! - [`jobs`] — what the runners' own hooks reported.
//! - [`status`] — what we ANSWER with: the verdict, and the machine-facing
//!   payload that carries it.
//! - [`timeline`] — the one group with a shape rather than a source: what
//!   CHANGED between two samples.
//!
//! That is the same seam already cut through [`crate::service::store::reader`]
//! and [`crate::service::store::writer`], which is the argument for it. A schema
//! change follows a producer, and it should touch one file in the read path, one
//! in the write path, and one here — not three files chosen on three different
//! principles.
//!
//! [`divergent`] stays in this parent rather than in either child because it
//! spans both: it takes a LOCAL [`Liveness`] and a GITHUB [`GhView`]. That is the
//! same placement it has in `metrics::encode`, and for the same reason — one
//! derivation shared by two consumers belongs above the cut, so the exporter and
//! the TUI header cannot disagree about who is diverging.
//!
//! Many of these are the shapes the store's read queries return, and they double
//! as the IPC wire payloads — the collector serves them, the TUI renders them. So
//! they live here in the shared domain rather than inside the service's store,
//! which is what lets the TUI depend on these types without depending on
//! `service::store`. That was previously said in a banner comment over one group;
//! it is true of the module, so it is said once, here.
//!
//! Every type is re-exported flat, so callers keep saying `models::GhView` and
//! have no reason to learn which source's file it moved to.

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
    /// Install directory as a string — the runner's locally-unique identity
    /// (agentId collides across orgs). Joins to `runner_state`.
    pub dir: String,
    pub name: String,
    pub org: String,
    pub liveness: Liveness,
    pub current_run_id: Option<i64>,
    pub cpu_pct: Option<f32>,
    /// Working-set memory (anon+shmem).
    pub mem_bytes: Option<u64>,
    /// Raw cgroup `memory.current` (working set + reclaimable page cache).
    pub mem_current_bytes: Option<u64>,
    pub uptime_s: Option<u64>,
}

/// Current per-runner liveness plus the timestamp of the last liveness *change*
/// (the "edge"). One row per runner, upserted by the writer; survives restarts,
/// so "Idle/Active for <dur>" = `now - since_ts`. Keyed by the install `dir`
/// (locally unique) — NOT agentId, which collides across orgs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerState {
    pub dir: String,
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

/// Local process healthy, but GitHub says this runner cannot take work.
///
/// Derived — deliberately NOT a fourth [`Liveness`] variant. `Liveness` is a
/// pure local-process fact and must stay one; folding GitHub's opinion into it
/// would conflate two independently useful signals and make this incident class
/// *less* diagnosable, not more. The whole point is to show both halves and
/// their disagreement.
///
/// `None` when the GitHub view is stale or unknown: not knowing is not the same
/// as diverging, and neither an alert nor a header may fire on ignorance.
///
/// Lives here, in the domain, rather than in the exporter, because the metrics
/// encoder AND the TUI header both need the same verdict — and two copies of
/// this reasoning would be exactly the "fix here, forgot there" bug class this
/// codebase already guards against.
pub fn divergent(liveness: Liveness, gh: GhView) -> Option<bool> {
    match (liveness, gh) {
        // Locally down is already visible in every other signal; calling it
        // "divergent" too would double-count the same outage.
        (Liveness::Offline, _) => Some(false),
        (_, GhView::Fresh { state, .. }) => Some(!state.online),
        (_, GhView::Stale { .. } | GhView::Unknown) => None,
    }
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
    /// Locally online (listener process present) — a purely local fact.
    pub online: u32,
    /// What GitHub said about this tick's runners. `None` when no runner had a
    /// fresh reading, which the chart must plot as a GAP rather than as zero:
    /// drawing "0 online" for "we didn't ask" invents an outage, and drawing
    /// the local line alone drew a flat healthy trace straight through a real
    /// one.
    pub github: Option<GhCount>,
}
