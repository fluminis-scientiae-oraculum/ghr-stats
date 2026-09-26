//! Read-only GitHub REST API over blocking `ureq`. Tokens are never logged.

mod scope;
pub mod validate;

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::shared::config::Secret;
use crate::shared::models::ApiErrorKind;

pub(crate) use scope::is_repo_of;
pub use scope::{GitHubHost, Owner, RunnerScope};

/// ureq's default timeout is infinite: a stalled peer would hang the producer thread and
/// block SIGTERM shutdown.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const PER_PAGE: usize = 100;
/// Bounds one listing at 5000 items.
const MAX_PAGES: usize = 50;

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
struct RunnersPage {
    #[serde(default)]
    runners: Vec<ApiRunner>,
}

/// One job of a run attempt. `conclusion` is null until it finishes; `runner_name` is
/// null until it is assigned.
#[derive(Debug, Clone, Deserialize)]
pub struct RunJob {
    pub name: String,
    #[serde(default)]
    pub runner_name: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Deserialize)]
struct JobsPage {
    #[serde(default)]
    jobs: Vec<RunJob>,
}

/// Every self-hosted runner in `scope`. Org scopes need "Self-hosted runners: read";
/// repository scopes need "Administration: read" on that repository.
pub fn runners(scope: &RunnerScope, token: &Secret) -> Result<Vec<ApiRunner>, ApiErrorKind> {
    let path = scope.runners_path().map_err(|_| ApiErrorKind::NotFound)?;
    paged(&scope.host, token, &path, |p: RunnersPage| p.runners)
}

/// The jobs of one attempt of a workflow run; needs "Actions: read".
pub fn attempt_jobs(
    host: &GitHubHost,
    token: &Secret,
    repo: &str,
    run_id: i64,
    attempt: i64,
) -> Result<Vec<RunJob>, ApiErrorKind> {
    let path = format!("/repos/{repo}/actions/runs/{run_id}/attempts/{attempt}/jobs");
    paged(host, token, &path, |p: JobsPage| p.jobs)
}

fn paged<P: DeserializeOwned, T>(
    host: &GitHubHost,
    token: &Secret,
    path: &str,
    items: impl Fn(P) -> Vec<T>,
) -> Result<Vec<T>, ApiErrorKind> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!("{}{path}?per_page={PER_PAGE}&page={page}", host.api_base());
        let batch = items(get_json(&url, token)?);
        let last = batch.len() < PER_PAGE;
        all.extend(batch);
        if last {
            break;
        }
    }
    Ok(all)
}

fn get_json<T: DeserializeOwned>(url: &str, token: &Secret) -> Result<T, ApiErrorKind> {
    let resp = ureq::get(url)
        .config()
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {}", token.expose()))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "ghr-stats")
        .call();
    match resp {
        Ok(mut r) => r.body_mut().read_json().map_err(|_| ApiErrorKind::Decode),
        Err(ureq::Error::StatusCode(code)) => Err(ApiErrorKind::from_status(code)),
        Err(_) => Err(ApiErrorKind::Transport),
    }
}

/// An operator-facing line for a failed call against `what`.
pub fn describe_failure(what: &str, kind: ApiErrorKind) -> String {
    match kind.http_status() {
        Some(code) => format!("{what}: HTTP {code} — {}", kind.hint()),
        None => format!("{what}: {}", kind.hint()),
    }
}
