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

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod timeline;

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

/// A runner's state as GitHub reports it (from the API reconcile pass).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRunnerRow {
    pub agent_id: i64,
    pub org: String,
    pub name: String,
    pub online: bool,
    pub busy: bool,
}

/// Why a per-org GitHub reconcile produced no data.
///
/// The SINGLE taxonomy: the Prometheus `kind` label, the stored `error_kind`,
/// and the operator-facing hint all derive from this one enum, so a metric and
/// a log line can never describe the same failure differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 401 — the token is invalid or expired.
    Unauthorized,
    /// 403 — the token lacks "Self-hosted runners: read", or org approval is
    /// still pending.
    Forbidden,
    /// 404 — org not found, or invisible to this token.
    NotFound,
    /// Any other HTTP status.
    Http(u16),
    /// Never reached GitHub: connection refused, TLS failure, timeout.
    Transport,
    /// A response arrived but did not decode.
    Decode,
}

impl ApiErrorKind {
    pub fn from_status(code: u16) -> Self {
        match code {
            401 => ApiErrorKind::Unauthorized,
            403 => ApiErrorKind::Forbidden,
            404 => ApiErrorKind::NotFound,
            other => ApiErrorKind::Http(other),
        }
    }

    /// Low-cardinality label for the `kind` dimension of
    /// `ghr_api_reconcile_errors_total`, and the stored `error_kind`.
    pub fn label(&self) -> String {
        match self {
            ApiErrorKind::Unauthorized => "http_401".to_string(),
            ApiErrorKind::Forbidden => "http_403".to_string(),
            ApiErrorKind::NotFound => "http_404".to_string(),
            ApiErrorKind::Http(code) => format!("http_{code}"),
            ApiErrorKind::Transport => "transport".to_string(),
            ApiErrorKind::Decode => "decode".to_string(),
        }
    }

    /// The actionable operator-facing explanation.
    pub fn hint(&self) -> &'static str {
        match self {
            ApiErrorKind::Unauthorized => "token is invalid or expired",
            ApiErrorKind::Forbidden => {
                "token lacks 'Self-hosted runners: read', or org approval is pending"
            }
            ApiErrorKind::NotFound => {
                "org not found, or this token cannot see it (wrong resource owner?)"
            }
            ApiErrorKind::Http(_) => "unexpected status",
            ApiErrorKind::Transport => "could not reach GitHub",
            ApiErrorKind::Decode => "response did not decode",
        }
    }

    /// The HTTP status, when the failure had one.
    pub fn http_status(&self) -> Option<u16> {
        match self {
            ApiErrorKind::Unauthorized => Some(401),
            ApiErrorKind::Forbidden => Some(403),
            ApiErrorKind::NotFound => Some(404),
            ApiErrorKind::Http(code) => Some(*code),
            ApiErrorKind::Transport | ApiErrorKind::Decode => None,
        }
    }
}

/// One org's outcome for a single reconcile tick.
///
/// An enum rather than a struct carrying an `ok` flag beside the rows: runner
/// rows exist ONLY in the success arm, so "a failed fetch moved the GitHub
/// liveness edge" is *unrepresentable* rather than merely forbidden. That
/// guard matters because the mistake it prevents is silent — treating an
/// unreachable org as "every one of its runners went offline" would invent an
/// outage out of a network blip.
///
/// `Unconfigured` is deliberately distinct from `Failed`: an org with no PAT
/// must report as "not configured" — not as an error, and not by vanishing —
/// so an operator can tell "I never set this up" from "my token broke".
#[derive(Debug, Clone)]
pub enum ApiOrgOutcome {
    Ok {
        org: String,
        rows: Vec<ApiRunnerRow>,
    },
    Failed {
        org: String,
        kind: ApiErrorKind,
    },
    Unconfigured {
        org: String,
    },
}

impl ApiOrgOutcome {
    pub fn org(&self) -> &str {
        match self {
            ApiOrgOutcome::Ok { org, .. }
            | ApiOrgOutcome::Failed { org, .. }
            | ApiOrgOutcome::Unconfigured { org } => org,
        }
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiState {
    pub online: bool,
    pub busy: bool,
}

/// GitHub's view of one runner, with freshness ALREADY adjudicated.
///
/// The reader applies the age bound once and hands out this verdict, so no
/// downstream consumer can read a six-hour-old row as though it were live. That
/// is the whole point: the exporter, the TUI and the status verb each used to
/// receive a bare `ApiState` and would each have had to remember to check a
/// timestamp. Three consumers, three chances to forget.
///
/// `Stale` and `Unknown` are deliberately distinct. "We knew, but the data has
/// aged out" and "we have never had data for this runner" call for different
/// operator responses, and collapsing them — which is exactly what an
/// `Option<ApiState>` does — is how a dead reconcile came to present as a calm
/// fleet.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum GhView {
    /// Within the freshness window; `age_s` is how old the reading is.
    Fresh { state: ApiState, age_s: i64 },
    /// We have a reading, but it is older than the window allows.
    Stale { age_s: i64 },
    /// No reconcile row for this runner at all.
    Unknown,
}

impl GhView {
    /// Adjudicate one reading against the freshness window: the single place the
    /// Fresh/Stale call is made.
    ///
    /// `age_s` is how old the reading was *at the instant being described* —
    /// now, for the live view; the sample's own tick, for a historical one. The
    /// rule is the same either way, which is the point: a `timeline` row cannot
    /// present as live something the live path would have called stale.
    ///
    /// The comparison happens in `u64` space. Casting `max_age` down to `i64`
    /// would wrap a large window (`u64::MAX` ⇒ -1) and mark every reading stale
    /// — the exact inverse of the intent. Clamping `age_s` non-negative first
    /// makes widening it lossless.
    pub fn observed(state: ApiState, age_s: i64, max_age: u64) -> GhView {
        let age_s = age_s.max(0);
        if age_s as u64 <= max_age {
            GhView::Fresh { state, age_s }
        } else {
            GhView::Stale { age_s }
        }
    }

    /// GitHub's online bit, but only when it is trustworthy. `None` for stale
    /// or unknown — callers must not treat "we don't know" as "offline".
    pub fn online(&self) -> Option<bool> {
        match self {
            GhView::Fresh { state, .. } => Some(state.online),
            GhView::Stale { .. } | GhView::Unknown => None,
        }
    }

    /// GitHub's busy bit, on the same terms as [`Self::online`].
    pub fn busy(&self) -> Option<bool> {
        match self {
            GhView::Fresh { state, .. } => Some(state.busy),
            GhView::Stale { .. } | GhView::Unknown => None,
        }
    }

    /// Age of the underlying reading, fresh or stale. `None` when there has
    /// never been one — exported as `ghr_runner_github_sample_age_seconds` so a
    /// scrape can see staleness building rather than only its aftermath.
    pub fn age_s(&self) -> Option<i64> {
        match self {
            GhView::Fresh { age_s, .. } | GhView::Stale { age_s } => Some(*age_s),
            GhView::Unknown => None,
        }
    }
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

/// GitHub-side liveness edge for one runner: mirrors [`RunnerState`], but keyed
/// by the API join key `(org, agent_id)`. `since_ts` is the last time `online`
/// actually CHANGED, which is what makes "offline to GitHub for >15m" an
/// alertable quantity instead of a bit that flaps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRunnerState {
    pub org: String,
    pub agent_id: i64,
    pub online: bool,
    pub since_ts: i64,
    pub last_seen_ts: i64,
}

/// Current health of one org's GitHub reconcile. Exists so that "GitHub says
/// offline" and "we could not ask GitHub" are separately observable — without
/// it, a dead reconcile presents as a calm fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiReconcileState {
    pub org: String,
    /// Last tick that SUCCEEDED. `None` if this org has never succeeded.
    pub last_ok_ts: Option<i64>,
    pub last_try_ts: i64,
    /// Outcome of the most recent attempt.
    pub ok: bool,
    pub http_status: Option<u16>,
    /// [`ApiErrorKind::label`] of the last failure, if any.
    pub error_kind: Option<String>,
    /// Whether a PAT is configured for this org at all. `false` reports as
    /// "not configured" rather than as an error or an absence.
    pub configured: bool,
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

/// GitHub's count for one occupancy tick, carried with the population it can
/// actually speak for.
///
/// A bare count is ambiguous, and on a real fleet the ambiguity is permanent:
/// a host with an org that can never be reconciled (a personal account has no
/// org runner API) will always have runners GitHub was never asked about. Plot
/// only `online` and the GitHub line sits forever below the local one — a
/// standing outage that is really just a standing silence. `known` is the
/// denominator that tells "GitHub says 9 of the 17 it knows are online" apart
/// from "9 of 17, and 4 more we never asked".
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GhCount {
    /// Runners GitHub considered online, out of `known`.
    pub online: u32,
    /// Runners at this tick that had a reading fresh enough to count.
    pub known: u32,
}

impl GhCount {
    /// A tick's GitHub count, or `None` when nothing at this tick had a fresh
    /// reading.
    ///
    /// The gap/zero decision lives HERE rather than at each call site: `known
    /// == 0` is the "nobody asked" case, and it must reach the chart as an
    /// absent point. Emitting `GhCount { online: 0, known: 0 }` would draw
    /// "GitHub says nothing is online" for a fleet nobody asked GitHub about —
    /// inventing an outage instead of admitting ignorance. Returning `Option`
    /// makes the empty count unrepresentable rather than merely discouraged.
    pub fn new(online: u32, known: u32) -> Option<Self> {
        (known > 0).then_some(Self { online, known })
    }
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
