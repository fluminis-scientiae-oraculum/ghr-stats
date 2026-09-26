//! PAT validation: fine-grained `github_pat_` tokens only, then read the org's runners and
//! match agentIds. GitHub offers no introspection of a fine-grained token's grants, so this
//! is the achievable check.

use std::collections::HashSet;

use super::{GitHubHost, Owner, RunnerScope, describe_failure, runners};
use crate::shared::config::Secret;

const FINE_PREFIX: &str = "github_pat_";
const CLASSIC_PREFIXES: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
const GUIDANCE: &str = "use a FINE-GRAINED token (github_pat_…) with Organization → \
     Self-hosted runners: Read, or Repository → Administration: Read for repository \
     runners (+ Repository → Actions: Read for job results)";

pub(crate) enum PatCheck {
    /// Authenticated; `matched` of `local` discovered runners were confirmed.
    Valid {
        runners: usize,
        matched: usize,
        local: usize,
    },
    Rejected(String),
}

pub(crate) fn prefix_check(token: &str) -> Result<(), String> {
    let t = token.trim();
    if t.starts_with(FINE_PREFIX) {
        return Ok(());
    }
    if CLASSIC_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return Err(format!("classic token detected — {GUIDANCE}"));
    }
    Err(format!("unrecognized token — {GUIDANCE}"))
}

/// Validate `token` for `org` against this host's runners, given as `(scope, agentId)`:
/// every scope registered under `org` is listed, and only `org`'s runners are matched.
pub(crate) fn validate(token: &str, org: &str, local: &[(RunnerScope, i64)]) -> PatCheck {
    if let Err(g) = prefix_check(token) {
        return PatCheck::Rejected(g);
    }
    let mine: Vec<&(RunnerScope, i64)> = local
        .iter()
        .filter(|(s, _)| s.login().eq_ignore_ascii_case(org))
        .collect();
    let mut scopes: Vec<RunnerScope> = mine.iter().map(|(s, _)| s.clone()).collect();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        scopes.push(RunnerScope {
            host: GitHubHost::dotcom(),
            owner: Owner::Org(org.to_string()),
        });
    }
    let secret = Secret::from(token.trim().to_string());
    let mut api = Vec::new();
    for scope in &scopes {
        match runners(scope, &secret) {
            Ok(r) => api.extend(r),
            Err(kind) => return PatCheck::Rejected(describe_failure(org, kind)),
        }
    }
    let ids: HashSet<i64> = mine.iter().map(|(_, id)| *id).collect();
    PatCheck::Valid {
        runners: api.len(),
        matched: api.iter().filter(|r| ids.contains(&r.id)).count(),
        local: ids.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fine_grained_passes_prefix() {
        assert!(prefix_check("github_pat_ABC").is_ok());
        assert!(prefix_check("  github_pat_ABC  ").is_ok());
    }

    #[test]
    fn classic_is_rejected_with_guidance() {
        for p in ["ghp_x", "gho_x", "ghu_x", "ghs_x", "ghr_x"] {
            let e = prefix_check(p).unwrap_err();
            assert!(e.contains("classic"), "{e}");
            assert!(e.contains("github_pat_"));
            assert!(e.contains("Self-hosted runners: Read"));
        }
    }

    #[test]
    fn garbage_is_rejected() {
        let e = prefix_check("hunter2").unwrap_err();
        assert!(e.contains("unrecognized"));
        assert!(e.contains("github_pat_"));
    }
}
