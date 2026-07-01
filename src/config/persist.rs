//! Faithful in-place config edits for the TUI's Config actions.
//!
//! Each edit loads the config TOML as data, changes exactly one setting, and
//! writes it back `0600` — preserving every OTHER setting. This is the opposite
//! of rebuilding the file from a fresh template (which would reset intervals or
//! drop the push config). It is not format-preserving (comments are lost), but
//! once you edit via the TUI the file is machine-managed anyway.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use toml::{Table, Value};

use crate::error::{Error, Result};

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
        .map_err(|e| Error::Config(format!("opening {}: {e}", target.display())))?;
    f.write_all(body.as_bytes())
        .map_err(|e| Error::Config(format!("writing {}: {e}", target.display())))?;
    // Enforce 0600 even if the file pre-existed with looser perms — it holds a PAT.
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600)).ok();
    // If a sudo TUI wrote it, hand it back to the invoking user (matches the CLI).
    crate::paths::chown_to_invoker(target);
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
    use crate::config::Config;

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
