//! PAT validation: fine-grained `github_pat_` tokens only, then read the org's runners and
//! match agentIds. GitHub offers no introspection of a fine-grained token's grants, so this
//! is the achievable check.

use std::collections::HashSet;

use super::list_org_runners;

const FINE_PREFIX: &str = "github_pat_";
const CLASSIC_PREFIXES: [&str; 5] = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
const GUIDANCE: &str = "use a FINE-GRAINED token (github_pat_…) with Organization → \
     Self-hosted runners: Read (+ Repository → Actions: Read for job results)";

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

pub(crate) fn validate(token: &str, org: &str, local_ids: &HashSet<i64>) -> PatCheck {
    if let Err(g) = prefix_check(token) {
        return PatCheck::Rejected(g);
    }
    match list_org_runners(token, org) {
        Ok(api) => {
            let matched = api.iter().filter(|r| local_ids.contains(&r.id)).count();
            PatCheck::Valid {
                runners: api.len(),
                matched,
                local: local_ids.len(),
            }
        }
        Err(e) => PatCheck::Rejected(e.to_string()),
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
