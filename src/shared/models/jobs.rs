//! What the runners' own hooks reported.

use serde::{Deserialize, Serialize};

/// Hook timing joined with the API conclusion once resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRow {
    pub runner_name: String,
    pub repo: String,
    pub job: String,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub conclusion: Option<String>,
}

/// Completed `job_event` whose conclusion is not yet resolved from the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConclusion {
    pub org: String,
    pub repo: String,
    pub run_id: i64,
    pub run_attempt: i64,
    pub job: String,
    pub runner_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobConclusion {
    pub run_id: i64,
    pub run_attempt: i64,
    pub job: String,
    pub runner_name: String,
    pub conclusion: String,
}
