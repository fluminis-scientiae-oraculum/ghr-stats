//! What GitHub said, and whether we could ask.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRunnerRow {
    pub agent_id: i64,
    pub org: String,
    pub name: String,
    pub online: bool,
    pub busy: bool,
}

/// Why a per-org reconcile produced no data; source of the metric `kind` label, the stored
/// `error_kind` and the operator hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    Unauthorized,
    Forbidden,
    NotFound,
    Http(u16),
    /// Never reached GitHub: connection refused, TLS failure, timeout.
    Transport,
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

    /// Low-cardinality `kind` label of `ghr_api_reconcile_error`; also the stored
    /// `error_kind`.
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

    pub fn hint(&self) -> &'static str {
        match self {
            ApiErrorKind::Unauthorized => "token is invalid or expired",
            ApiErrorKind::Forbidden => {
                "token lacks 'Self-hosted runners: read' (or 'Administration: read' on a \
                 repository), or org approval is pending"
            }
            ApiErrorKind::NotFound => {
                "org or repository not found, or this token cannot see it (wrong resource owner?)"
            }
            ApiErrorKind::Http(_) => "unexpected status",
            ApiErrorKind::Transport => "could not reach GitHub",
            ApiErrorKind::Decode => "response did not decode",
        }
    }

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

/// One org's outcome for a reconcile tick. Rows exist only in `Ok`, so a failed fetch can't
/// move the liveness edge. `Unconfigured` (no PAT) is reported apart from `Failed`.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiState {
    pub online: bool,
    pub busy: bool,
}

/// GitHub's view of one runner with freshness already decided by the reader.
/// `Stale` (aged out) and `Unknown` (never read) need different operator responses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum GhView {
    Fresh { state: ApiState, age_s: i64 },
    Stale { age_s: i64 },
    Unknown,
}

impl GhView {
    /// The single Fresh/Stale decision. `age_s` is the reading's age at the instant described
    /// (now, or a historical tick). Compared in `u64`: casting `max_age` to `i64` would wrap.
    pub fn observed(state: ApiState, age_s: i64, max_age: u64) -> GhView {
        let age_s = age_s.max(0);
        if age_s as u64 <= max_age {
            GhView::Fresh { state, age_s }
        } else {
            GhView::Stale { age_s }
        }
    }

    /// `None` unless fresh: "don't know" is not "offline".
    pub fn online(&self) -> Option<bool> {
        match self {
            GhView::Fresh { state, .. } => Some(state.online),
            GhView::Stale { .. } | GhView::Unknown => None,
        }
    }

    pub fn busy(&self) -> Option<bool> {
        match self {
            GhView::Fresh { state, .. } => Some(state.busy),
            GhView::Stale { .. } | GhView::Unknown => None,
        }
    }

    pub fn age_s(&self) -> Option<i64> {
        match self {
            GhView::Fresh { age_s, .. } | GhView::Stale { age_s } => Some(*age_s),
            GhView::Unknown => None,
        }
    }
}

/// GitHub-side liveness edge keyed by `(org, agent_id)`; `since_ts` is when `online` last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRunnerState {
    pub org: String,
    pub agent_id: i64,
    pub online: bool,
    pub since_ts: i64,
    pub last_seen_ts: i64,
}

/// Health of one org's reconcile, so "GitHub says offline" and "couldn't ask" stay distinguishable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiReconcileState {
    pub org: String,
    pub last_ok_ts: Option<i64>,
    pub last_try_ts: i64,
    /// Outcome of the latest attempt.
    pub ok: bool,
    pub http_status: Option<u16>,
    /// [`ApiErrorKind::label`] of the last failure.
    pub error_kind: Option<String>,
    pub configured: bool,
}

/// GitHub's count for one occupancy tick. `known` is the denominator: some runners (e.g. under
/// a personal account, which has no org runner API) are never asked about.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GhCount {
    pub online: u32,
    /// Runners with a reading fresh enough to count.
    pub known: u32,
}

impl GhCount {
    /// `None` when `known == 0`: the chart must show a gap, not "0 online".
    pub fn new(online: u32, known: u32) -> Option<Self> {
        (known > 0).then_some(Self { online, known })
    }
}
