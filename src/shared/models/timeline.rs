//! The `timeline` payload: what changed in a window, and optionally the raw
//! per-tick samples underneath it.
//!
//! Lives in `shared::models` rather than beside the verb in `ops` because it
//! crosses the IPC wire — the collector holds the history, so it must build the
//! answer, and `service` may not reach sideways into `ops`. It is its own
//! module rather than more lines in `models/mod.rs`: these types are consumed
//! by exactly one query and nothing else in the domain refers to them.
//!
//! The organising idea is that **a timeline is edges, not samples**. Reading
//! causality out of 121 readings per org per hour is what made the 2026-07-25
//! outage take nine manual calls; the same window expressed as "these four
//! things changed, in this order" is two lines. Raw samples remain available
//! under `--samples` for when the edges are not enough, but they are never the
//! default, and every collection here is explicitly bounded.

use serde::{Deserialize, Serialize};

use crate::shared::models::{GhView, Liveness};

/// What a client asks for. One struct shared by the CLI, the `Query` variant and
/// the reader, so the filters cannot be described one way on the wire and
/// applied another way in SQL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineQuery {
    /// Inclusive lower bound of the window (epoch seconds). The CLI derives it
    /// from `--since`, having already applied the maximum-window cap — so a
    /// hostile or mistaken client can still ask for anything, which is why the
    /// collector bounds the ROW COUNT rather than trusting the window.
    pub since_ts: i64,
    /// Maximum rows per collection. Clamped collector-side.
    pub limit: usize,
    /// Restrict to one org.
    pub org: Option<String>,
    /// Restrict to one runner, by display name.
    pub runner: Option<String>,
    /// Include the raw per-tick samples. Off by default: they are one to two
    /// orders of magnitude larger than the edges they explain.
    pub samples: bool,
}

/// A bounded slice of a series, carrying whether it was cut.
///
/// A silent cap reads exactly like complete data, which is the failure this
/// whole bump exists to stop. Pairing the rows with the flag in one type means a
/// caller cannot receive the rows without also receiving the caveat — the
/// reader sets it at the point it truncates, and there is nowhere to drop it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bounded<T> {
    /// The rows kept, oldest → newest.
    pub items: Vec<T>,
    /// True when `limit` dropped rows. The rows kept are the NEWEST ones;
    /// narrow the window or raise the limit to reach the rest.
    pub limited: bool,
}

impl<T> Bounded<T> {
    /// Keep at most `limit` of `rows` (which arrive newest-first), then flip to
    /// chronological order for output. The single place truncation happens, so
    /// `limited` cannot disagree with `items`.
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

/// The window the answer covers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    /// Requested lower bound, ISO-8601 UTC.
    pub since: String,
    pub since_epoch: i64,
    /// Upper bound — when the collector answered.
    pub until_epoch: i64,
    /// The oldest sample actually held, when it is NEWER than `since_epoch` —
    /// i.e. the window reaches past what `db prune` has left. `None` when the
    /// whole requested window is covered.
    ///
    /// Without this a pruned window is indistinguishable from a quiet one: both
    /// return few rows, and only one of them means "nothing happened".
    pub truncated_at: Option<i64>,
}

/// One observed change. `org` is common to every kind of edge; `runner` is not,
/// so it lives inside the arms that have one rather than being an `Option` with
/// a rule attached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub ts: i64,
    /// ISO-8601 UTC, alongside the epoch — the payload is read by both agents
    /// and humans and neither should have to convert.
    pub at: String,
    pub org: String,
    pub edge: Edge,
}

/// What changed. The three signals are deliberately separate: local liveness is
/// a process fact, GitHub's view is a remote opinion, and the reconcile outcome
/// says whether we were in a position to hold that opinion at all. An incident
/// is diagnosed by their disagreement, so collapsing any two would destroy the
/// evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    /// A runner's local process liveness changed. Three-valued, so both ends
    /// are carried — unlike the boolean edges below, `from` is not derivable.
    Liveness {
        runner: String,
        from: Liveness,
        to: Liveness,
    },
    /// GitHub's view of a runner flipped. The previous value is `!online` by
    /// construction, so it is not carried: a field that can only ever hold one
    /// value is a field that can be wrong.
    GithubOnline { runner: String, online: bool },
    /// The org's reconcile started or stopped succeeding. Org-scoped, not
    /// runner-scoped — and the reason the other two edges are readable: eight
    /// runners going GitHub-offline means something different depending on
    /// whether we were still able to ask GitHub at the time.
    Reconcile(ReconcileEdge),
}

/// Which way a reconcile edge went. An enum rather than `ok: bool` plus error
/// fields, so "recovered, with an HTTP 401" — a state a struct would happily
/// represent — cannot be built.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileEdge {
    /// The org answered again after failing.
    Recovered,
    /// The org stopped answering, and why.
    Failed {
        error_kind: Option<String>,
        http_status: Option<u16>,
    },
}

/// One runner at one tick: the local truth, and GitHub's view *as of that
/// instant* — with its freshness already adjudicated against the tick, exactly
/// as the live path adjudicates against now. A reading taken an hour before the
/// tick is [`GhView::Stale`] here for the same reason it would be live: the
/// consumer never sees a bare timestamp it has to remember to check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePoint {
    pub ts: i64,
    pub org: String,
    pub runner: String,
    pub liveness: Liveness,
    pub github: GhView,
}

/// The whole answer. Built collector-side, printed client-side — the same
/// arrangement as [`crate::shared::models::FleetStatus`], so what an agent reads
/// is what the collector computed rather than a client's reassembly of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub window: Window,
    /// Always present — the point of the verb.
    pub transitions: Bounded<Transition>,
    /// `None` means "not requested", never "none found": an empty `Bounded` is
    /// the answer for a window in which nothing was sampled, and conflating the
    /// two would make `--samples` unfalsifiable.
    pub samples: Option<Bounded<TimelinePoint>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_keeps_the_tail_and_reports_the_cut() {
        // Rows arrive newest-first, as every reader in this codebase returns them.
        let b = Bounded::newest(vec![5, 4, 3, 2, 1], 3);
        // Kept the three NEWEST, emitted oldest → newest.
        assert_eq!(b.items, vec![3, 4, 5]);
        assert!(b.limited);
    }

    #[test]
    fn newest_under_the_limit_is_not_marked_limited() {
        let b = Bounded::newest(vec![3, 2, 1], 10);
        assert_eq!(b.items, vec![1, 2, 3]);
        assert!(!b.limited);
    }

    /// Exactly at the limit is NOT truncation. Off-by-one here would cry wolf on
    /// every full page, which trains a reader to ignore the flag.
    #[test]
    fn newest_at_exactly_the_limit_is_complete() {
        let b = Bounded::newest(vec![3, 2, 1], 3);
        assert_eq!(b.items, vec![1, 2, 3]);
        assert!(!b.limited);
    }
}
