//! GitHub API (read-only), via the blocking `ureq` client — no async runtime.
//! We hit `/orgs/{org}/actions/runners` directly. Tokens are fine-grained,
//! read-only, and never logged.

pub mod validate;

use std::time::Duration;

use serde::Deserialize;

use crate::shared::error::{Error, Result};

/// Read/write timeout for every GitHub call. ureq's default leaves
/// `timeout_read`/`timeout_write` unset (infinite), so a peer that accepts the
/// connection then stalls mid-response would hang the calling producer thread
/// indefinitely and block the collector's SIGTERM shutdown. Bound it.
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
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "ghr-stats")
        .call();

    match resp {
        Ok(r) => r
            .into_json::<RunnersResponse>()
            .map(|body| body.runners)
            .map_err(|e| Error::Github(format!("{org}: decoding response: {e}"))),
        Err(ureq::Error::Status(code, _)) => Err(Error::Github(describe_status(org, code))),
        Err(ureq::Error::Transport(t)) => {
            Err(Error::Github(format!("{org}: transport error: {t}")))
        }
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
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "ghr-stats")
        .call();
    match resp {
        Ok(r) => r
            .into_json::<JobsResponse>()
            .map(|body| body.jobs)
            .map_err(|e| Error::Github(format!("{repo} run {run_id}: decoding jobs: {e}"))),
        Err(ureq::Error::Status(code, _)) => {
            Err(Error::Github(format!("{repo} run {run_id}: HTTP {code}")))
        }
        Err(ureq::Error::Transport(t)) => {
            Err(Error::Github(format!("{repo} run {run_id}: transport error: {t}")))
        }
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
