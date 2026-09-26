//! Config-file half of the wizard. Every write is an in-place [`persist`] edit
//! that preserves every other setting in the file.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Result;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};

use crate::shared::config::persist;
use crate::shared::github::validate::{self, FineGrainedPat, PatCheck};
use crate::shared::github::{RunnerScope, TokenKey};
use crate::shared::models::RunnerInfo;

use super::confirm;

#[derive(Default)]
pub(super) struct TokenPlan {
    pub(super) set: BTreeMap<TokenKey, FineGrainedPat>,
    pub(super) remove: BTreeSet<TokenKey>,
}

impl TokenPlan {
    pub(super) fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }
}

/// Empty when unreadable: a non-root run against the root-owned `/etc` config
/// degrades to add-only.
pub(super) fn existing_token_keys(target: &Path) -> BTreeSet<TokenKey> {
    std::fs::read_to_string(target)
        .ok()
        .map(|t| {
            crate::shared::config::token_orgs(&t)
                .iter()
                .filter_map(|k| TokenKey::parse(k).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Candidates include keys that only hold a PAT, so a stale one can be removed.
pub(super) fn manage_tokens(
    theme: &ColorfulTheme,
    discovered: &[RunnerInfo],
    existing: &BTreeSet<TokenKey>,
) -> Result<TokenPlan> {
    let mut plan = TokenPlan::default();
    let mut candidates: BTreeSet<TokenKey> = discovered
        .iter()
        .map(|r| TokenKey::for_scope(&r.scope))
        .collect();
    candidates.extend(existing.iter().cloned());
    if candidates.is_empty()
        || !confirm(
            theme,
            "Manage read-only GitHub PATs now? (add / replace / remove)",
            false,
        )?
    {
        return Ok(plan);
    }
    let local: Vec<(RunnerScope, i64)> = discovered
        .iter()
        .map(|r| (r.scope.clone(), r.agent_id))
        .collect();
    for key in &candidates {
        let has = existing.iter().any(|e| e.matches(key.host(), key.login()));
        if has {
            let choice = Select::with_theme(theme)
                .with_prompt(format!("  {key} already has a PAT — action?"))
                .items(["Keep it", "Replace the PAT", "Remove it (forget this org)"])
                .default(0)
                .interact()?;
            match choice {
                1 => {
                    if let Some(t) = prompt_validated_pat(theme, key, &local)? {
                        plan.set.insert(key.clone(), t);
                    }
                }
                2 => {
                    plan.remove.insert(key.clone());
                    println!("    • will remove {key}'s PAT");
                }
                _ => {}
            }
        } else if confirm(theme, &format!("  Add a token for {key}?"), false)?
            && let Some(t) = prompt_validated_pat(theme, key, &local)?
        {
            plan.set.insert(key.clone(), t);
        }
    }
    Ok(plan)
}

fn prompt_validated_pat(
    theme: &ColorfulTheme,
    key: &TokenKey,
    local: &[(RunnerScope, i64)],
) -> Result<Option<FineGrainedPat>> {
    loop {
        let input = Password::with_theme(theme)
            .with_prompt(format!("  Paste fine-grained PAT for {key}"))
            .allow_empty_password(true)
            .interact()?;
        if input.trim().is_empty() {
            return Ok(None);
        }
        let why = match FineGrainedPat::parse(&input) {
            Err(why) => why,
            Ok(pat) => match validate::validate(&pat, key, local) {
                PatCheck::Valid {
                    runners,
                    matched,
                    local,
                } => {
                    println!("    ✓ valid — {runners} runners, matched {matched}/{local} local");
                    return Ok(Some(pat));
                }
                PatCheck::Rejected(why) => why,
            },
        };
        println!("    ✗ {why}");
        if !confirm(theme, "    try again?", true)? {
            return Ok(None);
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
    for (key, token) in &plan.set {
        persist::set_org_token(target, key, token)?;
    }
    for key in &plan.remove {
        persist::remove_org_token(target, key)?;
    }
    // Declining leaves any existing pull/push config alone.
    if let MetricsChoice::Pull(addr) = metrics {
        persist::set_metrics_pull(target, true, Some(*addr))?;
    }
    Ok(())
}

pub(super) enum MetricsChoice {
    Unchanged,
    Pull(SocketAddr),
}

pub(super) fn prompt_metrics(theme: &ColorfulTheme) -> Result<MetricsChoice> {
    if !confirm(
        theme,
        "Expose Prometheus /metrics? (served by the collector service)",
        false,
    )? {
        return Ok(MetricsChoice::Unchanged);
    }
    let addr: SocketAddr = Input::with_theme(theme)
        .with_prompt("  metrics bind address (loopback unless scraped remotely)")
        .default(SocketAddr::from(([127, 0, 0, 1], 9477)))
        .interact_text()?;
    Ok(MetricsChoice::Pull(addr))
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

        let key = |s: &str| TokenKey::parse(s).unwrap();
        let pat = |s: &str| FineGrainedPat::parse(s).unwrap();
        let mut plan = TokenPlan::default();
        plan.set.insert(key("ACME"), pat("github_pat_NEW"));
        plan.set.insert(key("beta"), pat("github_pat_B"));
        plan.remove.insert(key("widgets"));
        let metrics = MetricsChoice::Unchanged;

        apply_config(&path, &[PathBuf::from("/srv/r")], &plan, &metrics).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: crate::shared::config::Config = toml::from_str(&text).unwrap();
        // Per-org tokens take precedence over env/fallback, so these are deterministic.
        assert_eq!(cfg.dotcom_token("acme").as_deref(), Some("github_pat_NEW"));
        assert_eq!(cfg.dotcom_token("beta").as_deref(), Some("github_pat_B"));
        assert!(
            !cfg.github
                .tokens
                .contains_key(&TokenKey::parse("widgets").unwrap())
        );
        assert!(
            !cfg.github
                .tokens
                .contains_key(&TokenKey::parse("acme").unwrap()),
            "replaced under the new spelling"
        );
        assert!(!text.contains("github_pat_W"));
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn existing_token_keys_reads_configured_keys_empty_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(existing_token_keys(&path).is_empty()); // no file yet
        std::fs::write(
            &path,
            "[github.tokens]\nacme = \"github_pat_A\"\nwidgets = \"github_pat_W\"\n",
        )
        .unwrap();
        let got = existing_token_keys(&path);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&TokenKey::parse("acme").unwrap()));
    }
}
