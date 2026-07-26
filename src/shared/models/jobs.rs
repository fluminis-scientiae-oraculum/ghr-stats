//! What the runners' own hooks reported.
//!
//! A job is the one fact here that arrives INCOMPLETE and is finished later by a
//! different producer, and the three types are that lifecycle. [`JobRow`] is the
//! joined view: hook timing now, an API conclusion eventually.
//! [`PendingConclusion`] is the reconcile's work-list — the rows the hook could
//! not finish. [`JobConclusion`] is the answer written back.
//!
//! `PendingConclusion` and `JobConclusion` are deliberately NOT serialisable:
//! they are internal to the collector's reconcile pass and never cross the wire,
//! which is the distinction that decides where a type lives in this codebase.

use serde::{Deserialize, Serialize};

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
