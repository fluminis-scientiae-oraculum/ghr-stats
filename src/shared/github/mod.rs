//! Read-only GitHub API over blocking `ureq`. Tokens are never logged.

pub mod validate;

use std::time::Duration;

use serde::Deserialize;

use crate::shared::error::{Error, Result};
use crate::shared::models::ApiErrorKind;

/// ureq's default timeout is infinite: a stalled peer would hang the producer thread and
/// block SIGTERM shutdown.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// `id` is the `.runner` `agentId`.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiRunner {
    pub id: i64,
    pub name: String,
    /// "online" | "offline".
    pub status: String,
    pub busy: bool,
}

#[derive(Deserialize)]
struct RunnersResponse {
    #[serde(default)]
    runners: Vec<ApiRunner>,
}

pub fn list_org_runners_classified(
    token: &str,
    org: &str,
) -> std::result::Result<Vec<ApiRunner>, ApiErrorKind> {
    let url = format!("https://api.github.com/orgs/{org}/actions/runners?per_page=100");
    let resp = ureq::get(&url)
        .config()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "ghr-stats")
        .call();

    match resp {
        Ok(mut r) => r
            .body_mut()
            .read_json::<RunnersResponse>()
            .map(|body| body.runners)
            .map_err(|_| ApiErrorKind::Decode),
        Err(ureq::Error::StatusCode(code)) => Err(ApiErrorKind::from_status(code)),
        Err(_) => Err(ApiErrorKind::Transport),
    }
}

/// Needs only the fine-grained "Self-hosted runners: read" org permission.
pub fn list_org_runners(token: &str, org: &str) -> Result<Vec<ApiRunner>> {
    list_org_runners_classified(token, org)
        .map_err(|kind| Error::Github(describe_failure(org, kind)))
}

/// `conclusion` is null until the job finishes.
#[derive(Debug, Clone, Deserialize)]
pub struct RunJob {
    pub name: String,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Deserialize)]
struct JobsResponse {
    #[serde(default)]
    jobs: Vec<RunJob>,
}

/// `repo` is `owner/name`. Needs "Actions: read"; a runners-only token gets 403, which
/// callers treat as skip.
pub fn list_run_jobs(token: &str, repo: &str, run_id: i64) -> Result<Vec<RunJob>> {
    let url =
        format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100");
    let resp = ureq::get(&url)
        .config()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "ghr-stats")
        .call();
    match resp {
        Ok(mut r) => r
            .body_mut()
            .read_json::<JobsResponse>()
            .map(|body| body.jobs)
            .map_err(|e| Error::Github(format!("{repo} run {run_id}: decoding jobs: {e}"))),
        Err(ureq::Error::StatusCode(code)) => {
            Err(Error::Github(format!("{repo} run {run_id}: HTTP {code}")))
        }
        Err(e) => Err(Error::Github(format!(
            "{repo} run {run_id}: transport error: {e}"
        ))),
    }
}

fn describe_failure(org: &str, kind: ApiErrorKind) -> String {
    match kind.http_status() {
        Some(code) => format!("{org}: HTTP {code} — {}", kind.hint()),
        None => format!("{org}: {}", kind.hint()),
    }
}
