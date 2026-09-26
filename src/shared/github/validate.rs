//! PAT validation: fine-grained `github_pat_` tokens only, then read the org's runners and
//! match agentIds. GitHub offers no introspection of a fine-grained token's grants, so this
//! is the achievable check.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{Owner, RunnerScope, TokenKey, describe_failure, runners};
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

/// A fine-grained PAT; built only by [`FineGrainedPat::parse`], so a classic token can
/// reach neither the config nor the wire.
#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FineGrainedPat(String);

impl FineGrainedPat {
    pub fn parse(token: &str) -> Result<Self, String> {
        let t = token.trim();
        if t.starts_with(FINE_PREFIX) {
            return Ok(Self(t.to_string()));
        }
        if CLASSIC_PREFIXES.iter().any(|p| t.starts_with(p)) {
            return Err(format!("classic token detected — {GUIDANCE}"));
        }
        Err(format!("unrecognized token — {GUIDANCE}"))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for FineGrainedPat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FineGrainedPat(\"***\")")
    }
}

impl TryFrom<String> for FineGrainedPat {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        Self::parse(&s)
    }
}

impl From<FineGrainedPat> for String {
    fn from(p: FineGrainedPat) -> String {
        p.0
    }
}

/// Validate `token` for `key` against this host's runners, given as `(scope, agentId)`:
/// every scope registered under the key is listed, and only its runners are matched.
pub(crate) fn validate(
    token: &FineGrainedPat,
    key: &TokenKey,
    local: &[(RunnerScope, i64)],
) -> PatCheck {
    let mine: Vec<&(RunnerScope, i64)> = local
        .iter()
        .filter(|(s, _)| key.matches(&s.host, s.login()))
        .collect();
    let mut scopes: Vec<RunnerScope> = mine.iter().map(|(s, _)| s.clone()).collect();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        scopes.push(RunnerScope {
            host: key.host().clone(),
            owner: Owner::Org(key.login().to_string()),
        });
    }
    let secret = Secret::from(token.expose().to_string());
    let mut api = Vec::new();
    for scope in &scopes {
        match runners(scope, &secret) {
            Ok(r) => api.extend(r),
            Err(kind) => return PatCheck::Rejected(describe_failure(&key.to_string(), kind)),
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
        assert!(FineGrainedPat::parse("github_pat_ABC").is_ok());
        assert_eq!(
            FineGrainedPat::parse("  github_pat_ABC  ")
                .unwrap()
                .expose(),
            "github_pat_ABC"
        );
    }

    #[test]
    fn classic_is_rejected_with_guidance() {
        for p in ["ghp_x", "gho_x", "ghu_x", "ghs_x", "ghr_x"] {
            let e = FineGrainedPat::parse(p).unwrap_err();
            assert!(e.contains("classic"), "{e}");
            assert!(e.contains("github_pat_"));
            assert!(e.contains("Self-hosted runners: Read"));
        }
    }

    #[test]
    fn garbage_is_rejected() {
        let e = FineGrainedPat::parse("hunter2").unwrap_err();
        assert!(e.contains("unrecognized"));
        assert!(e.contains("github_pat_"));
    }
}
