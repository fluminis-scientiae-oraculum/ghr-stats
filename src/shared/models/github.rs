//! What GitHub said, and how well we could ask.
//!
//! Three kinds of fact, from one producer — the reconcile thread. The raw
//! per-runner reading ([`ApiRunnerRow`], [`ApiState`]), the persisted liveness
//! edge that mirrors the local one but is keyed by `(org, agent_id)`
//! ([`ApiRunnerState`]), and the health of the asking itself
//! ([`ApiErrorKind`], [`ApiOrgOutcome`], [`ApiReconcileState`]).
//!
//! Two of these are constructions rather than records, and both exist because of
//! the same outage. [`ApiOrgOutcome`] carries runner rows ONLY in its `Ok` arm,
//! so "a failed fetch moved the liveness edge" is unrepresentable rather than
//! merely forbidden. [`GhView`] has freshness ALREADY adjudicated, so no consumer
//! can read a stale reading as a current one by forgetting to check an age.
//!
//! [`ApiErrorKind`] is the SINGLE taxonomy: the Prometheus `kind` label, the
//! stored `error_kind` and the operator-facing hint all derive from it, so a
//! metric and a log line can never describe one failure differently.

use serde::{Deserialize, Serialize};

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
