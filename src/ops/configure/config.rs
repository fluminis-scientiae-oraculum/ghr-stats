//! Config-file half of the wizard. Every write is an in-place [`persist`] edit
//! that preserves every other setting in the file.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};

use crate::shared::config::persist;
use crate::shared::github::validate::{self, PatCheck};
use crate::shared::models::RunnerInfo;

use super::confirm;

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

/// Empty when unreadable: a non-root run against the root-owned `/etc` config
/// degrades to add-only.
pub(super) fn existing_token_orgs(target: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(target)
        .ok()
        .map(|t| crate::shared::config::token_orgs(&t).into_iter().collect())
        .unwrap_or_default()
}

/// Candidates include orgs that only hold a PAT, so a stale one can be removed.
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
            PatCheck::Valid {
                runners,
                matched,
                local,
            } => {
                println!("    ✓ valid — {runners} runners, matched {matched}/{local} local");
                return Ok(Some(token));
            }
            PatCheck::Rejected(why) => {
                println!("    ✗ {why}");
                if !confirm(theme, "    try again?", true)? {
                    return Ok(None);
                }
            }
        }
    }
}

/// Prompt-free: all consent has happened before this is called.
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
    // Declining leaves any existing pull/push config alone.
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
        assert!(!cfg.github.tokens.contains_key("widgets"));
        assert!(!text.contains("github_pat_W"));
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
