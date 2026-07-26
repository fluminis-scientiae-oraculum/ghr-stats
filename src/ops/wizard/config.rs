//! What ends up in the `0600` config file: the per-org token plan and the
//! metrics choice.
//!
//! Every write here is a faithful in-place [`persist`] edit, so each OTHER
//! setting in the file survives untouched — the wizard may be re-run against a
//! hand-edited config without flattening it.
//!
//! [`apply_config`] is deliberately PURE OF PROMPTS: all consent has already
//! happened by the time it is called, which is what makes the whole
//! set/replace/remove/preserve behaviour unit-testable end to end. Both of this
//! module's tests exercise exactly that.
//!
//! [`existing_token_orgs`] reads presence from the file TEXT rather than a parsed
//! schema, so it survives schema drift, and it degrades to add-only rather than
//! failing when the file cannot be read — the ordinary case for a non-root run
//! against a root-owned `/etc` config.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};

use crate::shared::config::persist;
use crate::shared::github::validate::{self, Verdict};
use crate::shared::models::RunnerInfo;

use super::confirm;

/// The per-org PAT changes the token step decided: orgs to set/replace (with the
/// validated token) and orgs to remove. Applied via faithful `persist` edits.
#[derive(Default)]
pub(super) struct TokenPlan {
    pub(super) set: BTreeMap<String, String>,
    pub(super) remove: BTreeSet<String>,
}

impl TokenPlan {
    pub(super) fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// The org logins that already have a PAT in `target` — presence only, read from
/// the file text (so it survives schema drift). Empty when the file is absent or
/// unreadable (a non-root run can't read the root-owned `/etc` config, so it
/// degrades to add-only rather than failing).
pub(super) fn existing_token_orgs(target: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(target)
        .ok()
        .map(|t| crate::shared::config::token_orgs(&t).into_iter().collect())
        .unwrap_or_default()
}

/// Per-org PAT management: for an org that already has a PAT, offer keep /
/// replace / remove; for one without, offer to add. Candidates are the union of
/// discovered orgs and orgs that already hold a PAT (so a stale one — whose
/// runners are gone — can still be removed). Bounded validation on set/replace.
pub(super) fn manage_tokens(
    theme: &ColorfulTheme,
    discovered: &[RunnerInfo],
    existing: &BTreeSet<String>,
) -> Result<TokenPlan> {
    let mut plan = TokenPlan::default();
    let mut candidates: BTreeSet<String> = discovered.iter().map(|r| r.org.clone()).collect();
    candidates.extend(existing.iter().cloned());
    if candidates.is_empty()
        || !confirm(
            theme,
            "Manage read-only GitHub PATs now? (add / replace / remove; needs 'Self-hosted runners: Read', + 'Actions: Read' for job results)",
            false,
        )?
    {
        return Ok(plan);
    }
    for org in &candidates {
        let local_ids: HashSet<i64> = discovered
            .iter()
            .filter(|r| &r.org == org)
            .map(|r| r.agent_id)
            .collect();
        if existing.contains(org) {
            let choice = Select::with_theme(theme)
                .with_prompt(format!("  {org} already has a PAT — action?"))
                .items(["Keep it", "Replace the PAT", "Remove it (forget this org)"])
                .default(0)
                .interact()?;
            match choice {
                1 => {
                    if let Some(t) = prompt_validated_pat(theme, org, &local_ids)? {
                        plan.set.insert(org.clone(), t);
                    }
                }
                2 => {
                    plan.remove.insert(org.clone());
                    println!("    • will remove {org}'s PAT");
                }
                _ => {}
            }
        } else if confirm(theme, &format!("  Add a token for {org}?"), false)?
            && let Some(t) = prompt_validated_pat(theme, org, &local_ids)?
        {
            plan.set.insert(org.clone(), t);
        }
    }
    Ok(plan)
}

/// Prompt for a fine-grained PAT and validate it (fine-grained only, read +
/// agentId-confirm). `Some(token)` once valid; `None` if left blank or the user
/// gives up after a rejection. Shared by the add and replace paths.
fn prompt_validated_pat(
    theme: &ColorfulTheme,
    org: &str,
    local_ids: &HashSet<i64>,
) -> Result<Option<String>> {
    loop {
        let token = Password::with_theme(theme)
            .with_prompt(format!("  Paste fine-grained PAT for {org}"))
            .interact()?;
        let token = token.trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        match validate::validate(&token, org, local_ids) {
            Verdict::Valid {
                runners,
                matched,
                local,
            } => {
                println!("    ✓ valid — {runners} runners, matched {matched}/{local} local");
                return Ok(Some(token));
            }
            Verdict::Rejected(why) => {
                println!("    ✗ {why}");
                if !confirm(theme, "    try again?", true)? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Apply the wizard's decisions as faithful in-place `persist` edits: set the
/// roots, set/replace the collected PATs, remove the ones marked for removal, and
/// enable metrics if chosen. Every OTHER setting in the file is preserved. Pure
/// of prompts (all consent happened already), so it is unit-testable end-to-end.
pub(super) fn apply_config(
    target: &Path,
    roots: &[PathBuf],
    plan: &TokenPlan,
    metrics: &MetricsChoice,
) -> Result<()> {
    persist::set_runner_roots(target, roots)?;
    for (org, token) in &plan.set {
        persist::set_org_token(target, org, token)?;
    }
    for org in &plan.remove {
        persist::remove_org_token(target, org)?;
    }
    // Only touch metrics when enabling — declining leaves any existing pull/push
    // config alone rather than clobbering it.
    if metrics.pull {
        persist::set_metrics_pull(target, true, &metrics.addr)?;
    }
    Ok(())
}

pub(super) struct MetricsChoice {
    pub(super) pull: bool,
    pub(super) addr: String,
}

pub(super) fn prompt_metrics(theme: &ColorfulTheme) -> Result<MetricsChoice> {
    let pull = confirm(
        theme,
        "Expose Prometheus /metrics on loopback? (served by the collector service)",
        false,
    )?;
    let addr = if pull {
        Input::with_theme(theme)
            .with_prompt("  metrics bind address (keep it on 127.0.0.1)")
            .default("127.0.0.1:9477".to_string())
            .interact_text()?
    } else {
        "127.0.0.1:9477".to_string()
    };
    Ok(MetricsChoice { pull, addr })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wizard's apply step, end-to-end against a real config file: set a new
    /// PAT, replace an existing one, remove another, and set the roots — while
    /// every untouched setting (here the push config) survives. This is the
    /// CLI-side of add/replace/remove, proven without the interactive prompts.
    #[test]
    fn apply_config_sets_replaces_removes_and_preserves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/old\"]\n\
             [github.tokens]\nacme = \"github_pat_OLD\"\nwidgets = \"github_pat_W\"\n\
             [metrics.push]\nenabled = true\n",
        )
        .unwrap();

        let mut plan = TokenPlan::default();
        plan.set.insert("acme".into(), "github_pat_NEW".into()); // replace
        plan.set.insert("beta".into(), "github_pat_B".into()); // add
        plan.remove.insert("widgets".into()); // remove
        let metrics = MetricsChoice {
            pull: false,
            addr: "127.0.0.1:9477".into(),
        };

        apply_config(&path, &[PathBuf::from("/srv/r")], &plan, &metrics).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: crate::shared::config::Config = toml::from_str(&text).unwrap();
        // Per-org tokens take precedence over env/fallback, so these are deterministic.
        assert_eq!(
            cfg.github_token_for("acme").as_deref(),
            Some("github_pat_NEW")
        );
        assert_eq!(
            cfg.github_token_for("beta").as_deref(),
            Some("github_pat_B")
        );
        // widgets removed (presence check is env-independent).
        assert!(!cfg.github.tokens.contains_key("widgets"));
        assert!(!text.contains("github_pat_W"));
        // Untouched settings + the new roots.
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn existing_token_orgs_reads_configured_orgs_empty_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(existing_token_orgs(&path).is_empty()); // no file yet
        std::fs::write(
            &path,
            "[github.tokens]\nacme = \"github_pat_A\"\nwidgets = \"github_pat_W\"\n",
        )
        .unwrap();
        let got = existing_token_orgs(&path);
        assert_eq!(got.len(), 2);
        assert!(got.contains("acme") && got.contains("widgets"));
    }
}
