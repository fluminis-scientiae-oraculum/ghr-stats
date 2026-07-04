//! GitHub API (read-only), via the blocking `ureq` client — no async runtime.
//! We hit `/orgs/{org}/actions/runners` directly. Tokens are fine-grained,
//! read-only, and never logged.

pub mod validate;

use std::time::Duration;

use serde::Deserialize;

use crate::shared::error::{Error, Result};

/// Global timeout for every GitHub call. ureq leaves the timeout unset
/// (infinite) by default, so a peer that accepts the connection then stalls
/// mid-response would hang the calling producer thread indefinitely and block
/// the collector's SIGTERM shutdown. Bound it (`timeout_global`).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// A runner as GitHub sees it. `id` is the same `agentId` stored in `.runner`,
/// so it joins directly to locally-discovered runners.
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

/// List an org's self-hosted runners. Requires only the fine-grained
/// "Self-hosted runners: read" organization permission.
pub fn list_org_runners(token: &str, org: &str) -> Result<Vec<ApiRunner>> {
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
            .map_err(|e| Error::Github(format!("{org}: decoding response: {e}"))),
        Err(ureq::Error::StatusCode(code)) => Err(Error::Github(describe_status(org, code))),
        Err(e) => Err(Error::Github(format!("{org}: transport error: {e}"))),
    }
}

/// One job of a workflow run, as the Actions API reports it. `conclusion` is
/// null until the job finishes (then "success" | "failure" | "cancelled" | …).
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

/// List the jobs of one workflow run (`repo` = "owner/name"). Used to reconcile
/// each `job_event`'s pass/fail conclusion. Requires the fine-grained
/// "Actions: read" repository permission — a token scoped only to
/// "Self-hosted runners: read" gets 403 here, which the caller treats as "skip"
/// so conclusions simply stay unresolved rather than failing the reconcile.
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

/// Map an HTTP status to an actionable message for the common fine-grained-PAT
/// failures.
fn describe_status(org: &str, code: u16) -> String {
    let hint = match code {
        401 => "token is invalid or expired",
        403 => "token lacks 'Self-hosted runners: read', or org approval is pending",
        404 => "org not found, or this token cannot see it (wrong resource owner?)",
        _ => "unexpected status",
    };
    format!("{org}: HTTP {code} — {hint}")
}
