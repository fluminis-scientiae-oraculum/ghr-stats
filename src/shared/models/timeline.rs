//! The `timeline` payload: what changed in a window (edges, not samples); every collection
//! is bounded.

use serde::{Deserialize, Serialize};

use crate::shared::models::{GhView, Liveness};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineQuery {
    /// Inclusive, epoch seconds. Not trusted: the collector bounds the row count, not the window.
    pub since_ts: i64,
    /// Max rows per collection; clamped collector-side.
    pub limit: usize,
    pub org: Option<String>,
    /// By display name.
    pub runner: Option<String>,
    pub samples: bool,
}

/// Rows plus whether a limit cut them, so a caller can't receive one without the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounded<T> {
    /// Oldest → newest.
    pub items: Vec<T>,
    /// `limit` dropped rows; the kept rows are the newest.
    pub limited: bool,
}

impl<T> Bounded<T> {
    /// `rows` arrive newest-first; keeps the newest `limit`, emitted oldest-first.
    pub fn newest(mut rows: Vec<T>, limit: usize) -> Self {
        let limited = rows.len() > limit;
        rows.truncate(limit);
        rows.reverse();
        Bounded {
            items: rows,
            limited,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    /// ISO-8601 UTC.
    pub since: String,
    pub since_epoch: i64,
    /// When the collector answered.
    pub until_epoch: i64,
    /// Oldest held sample when the window reaches past pruned history; `None` when fully covered.
    pub truncated_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub ts: i64,
    /// ISO-8601 UTC of `ts`.
    pub at: String,
    pub org: String,
    pub edge: Edge,
}

/// Local liveness, GitHub's view and reconcile outcome stay separate: incidents show as
/// their disagreement.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Liveness {
        runner: String,
        from: Liveness,
        to: Liveness,
    },
    /// The previous value is `!online`.
    GithubOnline { runner: String, online: bool },
    /// Org-scoped: whether we could ask GitHub at all.
    Reconcile(ReconcileEdge),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileEdge {
    Recovered,
    Failed {
        error_kind: Option<String>,
        http_status: Option<u16>,
    },
}

/// Kept out of [`Edge`] and bounded separately so job churn can't evict state edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub ts: i64,
    pub at: String,
    pub org: String,
    pub runner: String,
    pub repo: String,
    pub job: String,
    pub edge: JobEdge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEdge {
    Started,
    /// `conclusion` is `None` until the reconcile resolves it; no correction is sent later.
    Completed {
        conclusion: Option<String>,
    },
}

/// One runner at one tick; `github` freshness is judged against the tick, not now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePoint {
    pub ts: i64,
    pub org: String,
    pub runner: String,
    pub liveness: Liveness,
    pub github: GhView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub window: Window,
    pub transitions: Bounded<Transition>,
    pub jobs: Bounded<JobTransition>,
    /// `None` = not requested; an empty `Bounded` = nothing sampled.
    pub samples: Option<Bounded<TimelinePoint>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_keeps_the_tail_and_reports_the_cut() {
        let b = Bounded::newest(vec![5, 4, 3, 2, 1], 3);
        assert_eq!(b.items, vec![3, 4, 5]);
        assert!(b.limited);
    }

    #[test]
    fn newest_under_the_limit_is_not_marked_limited() {
        let b = Bounded::newest(vec![3, 2, 1], 10);
        assert_eq!(b.items, vec![1, 2, 3]);
        assert!(!b.limited);
    }

    #[test]
    fn newest_at_exactly_the_limit_is_complete() {
        let b = Bounded::newest(vec![3, 2, 1], 3);
        assert_eq!(b.items, vec![1, 2, 3]);
        assert!(!b.limited);
    }
}
