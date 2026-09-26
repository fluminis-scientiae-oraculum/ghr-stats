//! In-place config edits: load the TOML, change one setting, write it back `0600`.
//! Other settings survive; comments and formatting do not.

use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use toml::{Table, Value};

use crate::shared::error::{Error, Result};
use crate::shared::github::TokenKey;
use crate::shared::github::validate::FineGrainedPat;

/// Load-modify-write under an exclusive flock, since the CLI wizard, the TUI and the
/// collector's IPC handler edit the same file from separate processes. The lock is
/// best-effort (non-root can't create the sidecar in `/etc`); the write surfaces real errors.
fn edit(target: &Path, mutate: impl FnOnce(&mut Table) -> Result<()>) -> Result<()> {
    let _lock = acquire_lock(target); // held until the fn returns
    let mut doc = load_table(target)?;
    mutate(&mut doc)?;
    write_table(target, &doc)
}

/// Sidecar `<config>.lock`; `None` when it can't be taken.
fn acquire_lock(target: &Path) -> Option<Flock<std::fs::File>> {
    let lock_path = target.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    Flock::lock(file, FlockArg::LockExclusive).ok()
}

fn load_table(target: &Path) -> Result<Table> {
    if !target.exists() {
        return Ok(Table::new());
    }
    let text = std::fs::read_to_string(target)
        .map_err(|e| Error::Config(format!("reading {}: {e}", target.display())))?;
    toml::from_str(&text).map_err(|e| Error::Config(format!("parsing {}: {e}", target.display())))
}

/// Atomic replace via a random-named `0600` temp file in the same directory, then
/// `rename`: readers and crashes see the whole old or new file, never a truncated one.
fn write_table(target: &Path, doc: &Table) -> Result<()> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).ok();
    let body = toml::to_string_pretty(doc)
        .map_err(|e| Error::Config(format!("serializing config: {e}")))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".ghr-stats-config-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| stage_err(target, &e))?;
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600)).ok();
    tmp.write_all(body.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| Error::Config(format!("writing staged config: {e}")))?;
    tmp.persist(target)
        .map_err(|e| Error::Config(format!("installing {}: {}", target.display(), e.error)))?;
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600)).ok();
    Ok(())
}

fn stage_err(target: &Path, e: &std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        Error::Config(format!(
            "{} is the root-owned system config — re-run with sudo",
            target.display()
        ))
    } else {
        Error::Config(format!(
            "staging config write for {}: {e}",
            target.display()
        ))
    }
}

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

/// Add or replace the PAT for `key`, replacing any key spelled differently for the same
/// login and host.
pub(crate) fn set_org_token(target: &Path, key: &TokenKey, token: &FineGrainedPat) -> Result<()> {
    edit(target, |doc| {
        let tokens = nested_table(doc, &["github", "tokens"])?;
        tokens.retain(|k, _| !same_key(k, key));
        tokens.insert(key.to_string(), Value::String(token.expose().to_string()));
        Ok(())
    })
}

fn same_key(k: &str, key: &TokenKey) -> bool {
    TokenKey::parse(k).is_ok_and(|k| k.matches(key.host(), key.login()))
}

pub(crate) fn set_runner_roots(target: &Path, roots: &[PathBuf]) -> Result<()> {
    edit(target, |doc| {
        let arr = roots
            .iter()
            .map(|p| Value::String(p.display().to_string()))
            .collect();
        doc.insert("runner_roots".to_string(), Value::Array(arr));
        Ok(())
    })
}

/// Remove `key`'s PAT and drop its login from `orgs`; an error if neither was there.
pub(crate) fn remove_org_token(target: &Path, key: &TokenKey) -> Result<()> {
    edit(target, |doc| {
        let mut removed = false;
        if let Some(github) = doc.get_mut("github").and_then(Value::as_table_mut) {
            if let Some(tokens) = github.get_mut("tokens").and_then(Value::as_table_mut) {
                let before = tokens.len();
                tokens.retain(|k, _| !same_key(k, key));
                removed |= tokens.len() < before;
                if tokens.is_empty() {
                    github.remove("tokens");
                }
            }
            if github.is_empty() {
                doc.remove("github");
            }
        }
        if key.host().is_dotcom()
            && let Some(Value::Array(orgs)) = doc.get_mut("orgs")
        {
            let before = orgs.len();
            orgs.retain(|v| {
                !v.as_str()
                    .is_some_and(|o| o.eq_ignore_ascii_case(key.login()))
            });
            removed |= orgs.len() < before;
        }
        if removed {
            Ok(())
        } else {
            Err(Error::Config(format!("no PAT is configured for {key}")))
        }
    })
}

/// Turn `/metrics` on or off; `addr` also moves it, otherwise its address is kept.
pub(crate) fn set_metrics_pull(
    target: &Path,
    enabled: bool,
    addr: Option<SocketAddr>,
) -> Result<()> {
    edit(target, |doc| {
        let pull = nested_table(doc, &["metrics", "pull"])?;
        pull.insert("enabled".to_string(), Value::Boolean(enabled));
        if let Some(addr) = addr {
            pull.insert("addr".to_string(), Value::String(addr.to_string()));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> TokenKey {
        TokenKey::parse(s).unwrap()
    }

    fn pat(s: &str) -> FineGrainedPat {
        FineGrainedPat::parse(s).unwrap()
    }
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

        set_org_token(&path, &key("acme"), &pat("github_pat_ABC")).unwrap();

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.dotcom_token("acme").as_deref(), Some("github_pat_ABC"));
        assert_eq!(cfg.intervals.local_secs, 9);
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![std::path::PathBuf::from("/srv/r")]);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn set_runner_roots_preserves_existing_pats_and_push() {
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
        assert_eq!(cfg.dotcom_token("acme").as_deref(), Some("github_pat_KEEP"));
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

        remove_org_token(&path, &key("acme")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("github_pat_A"),
            "acme PAT still present:\n{text}"
        );
        assert!(
            text.contains("github_pat_W"),
            "widgets PAT was dropped:\n{text}"
        );
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(
            !cfg.github
                .tokens
                .contains_key(&TokenKey::parse("acme").unwrap())
        );
        assert!(
            cfg.github
                .tokens
                .contains_key(&TokenKey::parse("widgets").unwrap())
        );
        assert_eq!(cfg.orgs, vec!["widgets"]);
        assert!(cfg.metrics.push.enabled);
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn remove_last_org_token_cleans_the_empty_github_table_and_rejects_an_unknown_org() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "runner_roots = [\"/srv/r\"]\n[github.tokens]\nacme = \"github_pat_A\"\n",
        )
        .unwrap();

        remove_org_token(&path, &key("acme")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("github"),
            "empty github table should be gone:\n{text}"
        );
        assert!(remove_org_token(&path, &key("acme")).is_err());
    }

    #[test]
    fn concurrent_writers_do_not_lose_updates() {
        use std::sync::Arc;
        use std::thread;
        // flock is per open file description, so threads in one process still contend.
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("config.toml"));
        std::fs::write(&*path, "runner_roots = [\"/srv/r\"]\n").unwrap();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let p = Arc::clone(&path);
                thread::spawn(move || {
                    set_org_token(
                        &p,
                        &key(&format!("org{i}")),
                        &pat(&format!("github_pat_{i}")),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let cfg: Config = toml::from_str(&std::fs::read_to_string(&*path).unwrap()).unwrap();
        for i in 0..8 {
            assert!(
                cfg.github
                    .tokens
                    .contains_key(&TokenKey::parse(&format!("org{i}")).unwrap()),
                "org{i}'s PAT was lost to a concurrent writer"
            );
        }
        assert_eq!(cfg.runner_roots, vec![PathBuf::from("/srv/r")]);
    }

    #[test]
    fn creates_a_fresh_file_0600_making_parents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("etc").join("ghr-stats").join("config.toml");
        set_org_token(&path, &key("acme"), &pat("github_pat_ABC")).unwrap();
        let cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.dotcom_token("acme").as_deref(), Some("github_pat_ABC"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
