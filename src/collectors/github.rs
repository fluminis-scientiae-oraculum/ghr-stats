//! GitHub API (read-only), via the blocking `ureq` client — no async runtime.
//! We hit `/orgs/{org}/actions/runners` directly. Tokens are fine-grained,
//! read-only, and never logged.
//!
//! `ApiRunner` captures the full response shape; `os`/`labels` are consumed by
//! the daemon's reconcile pass (stored), not the `github` check command.
#![allow(dead_code)]

use serde::Deserialize;

use crate::error::{Error, Result};

/// A runner as GitHub sees it. `id` is the same `agentId` stored in `.runner`,
/// so it joins directly to locally-discovered runners.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiRunner {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub os: Option<String>,
    /// "online" | "offline".
    pub status: String,
    pub busy: bool,
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
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
