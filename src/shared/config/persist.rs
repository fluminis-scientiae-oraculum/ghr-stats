//! Faithful in-place config edits for the TUI's Config actions.
//!
//! Each edit loads the config TOML as data, changes exactly one setting, and
//! writes it back `0600` — preserving every OTHER setting. This is the opposite
//! of rebuilding the file from a fresh template (which would reset intervals or
//! drop the push config). It is not format-preserving (comments are lost), but
//! once you edit via the TUI the file is machine-managed anyway.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::shared::error::{Error, Result};

fn load_table(target: &Path) -> Result<Table> {
    if !target.exists() {
        return Ok(Table::new());
    }
    let text = std::fs::read_to_string(target)
        .map_err(|e| Error::Config(format!("reading {}: {e}", target.display())))?;
    toml::from_str(&text).map_err(|e| Error::Config(format!("parsing {}: {e}", target.display())))
}

fn write_table(target: &Path, doc: &Table) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = toml::to_string_pretty(doc)
        .map_err(|e| Error::Config(format!("serializing config: {e}")))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(target)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                Error::Config(format!(
                    "{} is the root-owned system config — re-run with sudo",
                    target.display()
                ))
            } else {
                Error::Config(format!("opening {}: {e}", target.display()))
            }
        })?;
    f.write_all(body.as_bytes())
        .map_err(|e| Error::Config(format!("writing {}: {e}", target.display())))?;
    // Enforce 0600 even if the file pre-existed with looser perms — it holds a PAT.
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(())
}

/// Descend into (creating as needed) a nested table like `["metrics", "pull"]`.
fn nested_table<'a>(doc: &'a mut Table, path: &[&str]) -> Result<&'a mut Table> {
    let mut cur = doc;
    for key in path {
        let entry = cur
            .entry(key.to_string())
            .or_insert_with(|| Value::Table(Table::new()));
        cur = entry
            .as_table_mut()
            .ok_or_else(|| Error::Config(format!("`{key}` is not a table")))?;
    }
    Ok(cur)
}

/// Add/replace a per-org read-only PAT under `[github.tokens]` (the native
/// wizard's write step).
pub(crate) fn set_org_token(target: &Path, org: &str, token: &str) -> Result<()> {
    let mut doc = load_table(target)?;
    nested_table(&mut doc, &["github", "tokens"])?
        .insert(org.to_string(), Value::String(token.to_string()));
    write_table(target, &doc)
}

/// Set the runner install roots (the CLI wizard's Step 1 result) under
/// `runner_roots`, preserving every OTHER setting — so re-running `config` never
/// drops an existing PAT, the push config, or custom intervals. Faithful edit,
/// never a template rewrite.
pub(crate) fn set_runner_roots(target: &Path, roots: &[PathBuf]) -> Result<()> {
    let mut doc = load_table(target)?;
    let arr = roots
        .iter()
        .map(|p| Value::String(p.display().to_string()))
        .collect();
    doc.insert("runner_roots".to_string(), Value::Array(arr));
    write_table(target, &doc)
}

/// Remove a per-org PAT and forget the org: drop `[github.tokens].<org>`, prune
/// `<org>` from any explicit `orgs` list, and tidy the now-empty `[github.tokens]`
/// / `[github]` tables. Every other setting is preserved. Faithful edit — the
/// inverse of [`set_org_token`]. Removing an absent org is a no-op (idempotent).
pub(crate) fn remove_org_token(target: &Path, org: &str) -> Result<()> {
    let mut doc = load_table(target)?;
    if let Some(github) = doc.get_mut("github").and_then(Value::as_table_mut) {
        if let Some(tokens) = github.get_mut("tokens").and_then(Value::as_table_mut) {
            tokens.remove(org);
            if tokens.is_empty() {
                github.remove("tokens"); // no dangling empty [github.tokens]
            }
        }
        if github.is_empty() {
            doc.remove("github");
        }
    }
    if let Some(Value::Array(orgs)) = doc.get_mut("orgs") {
        orgs.retain(|v| v.as_str() != Some(org));
    }
    write_table(target, &doc)
}

/// Toggle + address the Prometheus pull endpoint under `[metrics.pull]` (the
/// Config tab's `[m]` action). Preserves the address when only toggling.
pub(crate) fn set_metrics_pull(target: &Path, enabled: bool, addr: &str) -> Result<()> {
    let mut doc = load_table(target)?;
    let pull = nested_table(&mut doc, &["metrics", "pull"])?;
    pull.insert("enabled".to_string(), Value::Boolean(enabled));
    pull.insert("addr".to_string(), Value::String(addr.to_string()));
    write_table(target, &doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::config::Config;

    #[test]
    fn set_org_token_adds_and_preserves_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/srv/r\"]\n\
             [intervals]\nlocal_secs = 9\n\
             [metrics.push]\nenabled = true\nendpoint = \"http://x\"\n",
        )
        .unwrap();

        set_org_token(&path, "acme", "github_pat_ABC").unwrap();

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg.github_token_for("acme").as_deref(),
            Some("github_pat_ABC")
        );
        // Untouched settings survive the edit.
        assert_eq!(cfg.intervals.local_secs, 9);
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![std::path::PathBuf::from("/srv/r")]);
        // Secret-bearing file is 0600.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn set_runner_roots_preserves_existing_pats_and_push() {
        // The exact complaint: re-running the wizard (which now sets roots via a
        // faithful edit) must NOT drop an already-configured PAT or push config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/old\"]\n\
             [github.tokens]\nacme = \"github_pat_KEEP\"\n\
             [metrics.push]\nenabled = true\nendpoint = \"http://x\"\n",
        )
        .unwrap();

        set_runner_roots(&path, &[PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]).unwrap();

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg.runner_roots,
            vec![PathBuf::from("/srv/a"), PathBuf::from("/srv/b")]
        );
        // The PAT survives — the whole point.
        assert_eq!(
            cfg.github_token_for("acme").as_deref(),
            Some("github_pat_KEEP")
        );
        assert!(cfg.metrics.push.enabled);
    }

    #[test]
    fn remove_org_token_drops_pat_forgets_org_and_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/srv/r\"]\norgs = [\"acme\", \"widgets\"]\n\
             [github.tokens]\nacme = \"github_pat_A\"\nwidgets = \"github_pat_W\"\n\
             [metrics.push]\nenabled = true\n",
        )
        .unwrap();

        remove_org_token(&path, "acme").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("github_pat_A"), "acme PAT still present:\n{text}");
        assert!(text.contains("github_pat_W"), "widgets PAT was dropped:\n{text}");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(!cfg.github.tokens.contains_key("acme"));
        assert!(cfg.github.tokens.contains_key("widgets"));
        assert_eq!(cfg.orgs, vec!["widgets"]); // org forgotten from the reconcile list
        assert!(cfg.metrics.push.enabled); // untouched settings survive
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn remove_last_org_token_cleans_the_empty_github_table_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/srv/r\"]\n[github.tokens]\nacme = \"github_pat_A\"\n",
        )
        .unwrap();

        remove_org_token(&path, "acme").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("github"), "empty github table should be gone:\n{text}");
        // Removing an already-absent org is a harmless no-op.
        remove_org_token(&path, "acme").unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(cfg.github.tokens.is_empty());
    }

    #[test]
    fn creates_a_fresh_file_0600_making_parents() {
        let dir = tempfile::tempdir().unwrap();
        // A not-yet-existing file in a not-yet-existing dir: exercises the
        // `load_table` empty-doc branch + `write_table`'s `create_dir_all`.
        let path = dir.path().join("etc").join("ghr-stats").join("config.toml");
        set_org_token(&path, "acme", "github_pat_ABC").unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cfg.github_token_for("acme").as_deref(),
            Some("github_pat_ABC")
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
