//! `ghr-stats setup` — consent-first interactive configuration.
//!
//! Discovers orgs from each runner's `.runner`, optionally validates a
//! read-only fine-grained PAT per org against the API, and writes a `0600`
//! config. Nothing is read, sent, or stored without an explicit confirmation,
//! and the token is never echoed (masked entry + redacted preview).

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password};
use serde::Serialize;

use crate::collectors::{github, runners};

const DEFAULT_ROOT: &str = "/mnt/store/ghr";

pub fn run(config_override: Option<&Path>) -> Result<()> {
    let theme = ColorfulTheme::default();

    println!("ghr-stats setup\n");
    println!("This will:");
    println!("  • read each runner's .runner file under the root you choose");
    println!("  • optionally contact GitHub with a read-only token you provide (per org)");
    println!("  • write a config file (mode 0600) — only after you confirm\n");
    if !confirm(&theme, "Proceed?", true)? {
        println!("aborted.");
        return Ok(());
    }

    // 1) Discover runners.
    let root: String = Input::with_theme(&theme)
        .with_prompt("Runner install root")
        .default(DEFAULT_ROOT.to_string())
        .interact_text()?;
    let roots = vec![PathBuf::from(&root)];
    let discovered = runners::discover(&roots);
    let mut orgs: Vec<String> = discovered.iter().map(|r| r.org.clone()).collect();
    orgs.sort();
    orgs.dedup();
    if discovered.is_empty() {
        println!("⚠ no runners found under {root} (no .runner files).");
    } else {
        println!(
            "found {} runners across {} orgs: {}",
            discovered.len(),
            orgs.len(),
            orgs.join(", ")
        );
    }

    // 2) Per-org read-only PAT (optional).
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    if !orgs.is_empty()
        && confirm(
            &theme,
            "Add read-only GitHub PATs now? (optional; needs 'Self-hosted runners: read')",
            false,
        )?
    {
        for org in &orgs {
            if !confirm(&theme, &format!("  Token for {org}?"), false)? {
                continue;
            }
            let token = Password::with_theme(&theme)
                .with_prompt(format!("  Paste fine-grained PAT for {org}"))
                .interact()?;
            let token = token.trim().to_string();
            if token.is_empty() {
                continue;
            }
            match github::list_org_runners(&token, org) {
                Ok(api) => {
                    let local: HashSet<i64> = discovered
                        .iter()
                        .filter(|r| &r.org == org)
                        .map(|r| r.agent_id)
                        .collect();
                    let matched = api.iter().filter(|r| local.contains(&r.id)).count();
                    println!(
                        "    ✓ valid — {} runners, matched {}/{} local",
                        api.len(),
                        matched,
                        local.len()
                    );
                    tokens.insert(org.clone(), token);
                }
                Err(e) => {
                    println!("    ✗ {e}");
                    if confirm(&theme, "    store it anyway?", false)? {
                        tokens.insert(org.clone(), token);
                    }
                }
            }
        }
    }

    // 3) Write the config (with consent), tokens redacted in the preview.
    let target = config_target(config_override);
    if target.exists()
        && !confirm(
            &theme,
            &format!("{} exists — overwrite?", target.display()),
            false,
        )?
    {
        println!("kept existing config; nothing written.");
        return Ok(());
    }

    let redacted: BTreeMap<String, String> = tokens
        .keys()
        .map(|k| (k.clone(), "***".to_string()))
        .collect();
    println!("\nWill write {} (mode 0600):\n", target.display());
    println!("{}", render_config(&roots, &redacted));
    if !confirm(&theme, "Write it?", true)? {
        println!("aborted; nothing written.");
        return Ok(());
    }
    write_config(&target, &render_config(&roots, &tokens))?;
    println!("✓ wrote {}", target.display());
    println!("\nNext: build + enable the collector —");
    println!("  systemctl --user enable --now ghr-stats-collector.service");
    Ok(())
}

fn confirm(theme: &ColorfulTheme, prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::with_theme(theme)
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

#[derive(Serialize)]
struct OutConfig {
    runner_roots: Vec<String>,
    intervals: OutIntervals,
    #[serde(skip_serializing_if = "Option::is_none")]
    github: Option<OutGithub>,
}

#[derive(Serialize)]
struct OutIntervals {
    local_secs: u64,
    api_secs: u64,
}

#[derive(Serialize)]
struct OutGithub {
    tokens: BTreeMap<String, String>,
}

/// Render a config TOML via the serializer (proper escaping). Pure + tested.
fn render_config(roots: &[PathBuf], tokens: &BTreeMap<String, String>) -> String {
    let out = OutConfig {
        runner_roots: roots.iter().map(|p| p.display().to_string()).collect(),
        intervals: OutIntervals {
            local_secs: 5,
            api_secs: 60,
        },
        github: (!tokens.is_empty()).then(|| OutGithub {
            tokens: tokens.clone(),
        }),
    };
    let body = toml::to_string_pretty(&out).unwrap_or_default();
    format!("# ghr-stats config (written by `ghr-stats setup`). Keep mode 0600.\n\n{body}")
}

fn write_config(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    // Enforce 0600 even if the file pre-existed with looser permissions.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn config_target(config_override: Option<&Path>) -> PathBuf {
    if let Some(p) = config_override {
        return p.to_path_buf();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("ghr-stats/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_round_trips_into_config() {
        let mut tokens = BTreeMap::new();
        tokens.insert("pt-immer".to_string(), "github_pat_xyz".to_string());
        let toml = render_config(&[PathBuf::from("/mnt/store/ghr")], &tokens);

        assert!(toml.contains("[github.tokens]"));
        assert!(toml.contains("pt-immer"));
        // The generated config must load cleanly under the strict schema.
        let cfg: crate::config::Config = toml::from_str(&toml).expect("generated config parses");
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/mnt/store/ghr")]);
        assert_eq!(
            cfg.github_token_for("pt-immer").as_deref(),
            Some("github_pat_xyz")
        );
    }

    #[test]
    fn render_without_tokens_omits_github_table() {
        let toml = render_config(&[PathBuf::from("/x")], &BTreeMap::new());
        assert!(!toml.contains("[github"));
        let _cfg: crate::config::Config = toml::from_str(&toml).expect("parses");
    }
}
