//! Bounded PAT validation: require a fine-grained
//! `github_pat_` token, then confirm it can read the org's runners and that they
//! match locally-discovered runners by agentId. Reject anything else with the
//! exact minimal-permission guidance.
//!
//! GitHub exposes NO bearer-side introspection of a fine-grained token's
//! *granted* permissions, so a read-validate + agentId-confirm is the achievable
//! ceiling. We never accept classic tokens (the type whose write caps we could
//! not have checked), which dissolves that sub-problem rather than engineering
//! around it.

use std::collections::HashSet;

use super::list_org_runners;

const FINE_PREFIX: &str = "github_pat_";
const CLASSIC_PREFIXES: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
const GUIDANCE: &str = "use a FINE-GRAINED token (github_pat_…) with Organization → \
     Self-hosted runners: Read (+ Repository → Actions: Read for job results)";

/// The result of validating a PAT for an org.
pub(crate) enum Verdict {
    /// Authenticated; `matched` of `local` discovered runners were confirmed.
    Valid {
        runners: usize,
        matched: usize,
        local: usize,
    },
    Rejected(String),
}

/// Prefix gate — pure, no network. Accept only fine-grained tokens.
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

/// Full validation: prefix gate, then read the org's runners and agentId-confirm
/// against the locally-discovered runners.
pub(crate) fn validate(token: &str, org: &str, local_ids: &HashSet<i64>) -> Verdict {
    if let Err(g) = prefix_check(token) {
        return Verdict::Rejected(g);
    }
    match list_org_runners(token, org) {
        Ok(api) => {
            let matched = api.iter().filter(|r| local_ids.contains(&r.id)).count();
            Verdict::Valid {
                runners: api.len(),
                matched,
                local: local_ids.len(),
            }
        }
        Err(e) => Verdict::Rejected(e.to_string()),
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
