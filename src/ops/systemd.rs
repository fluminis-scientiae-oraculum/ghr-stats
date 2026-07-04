//! `ghr-stats systemd install|uninstall` — manage the `serve` service.
//!
//! Install copies the running binary to a stable absolute path (so a root unit
//! and a later `sudo ghr-stats` resolve the same file — the sudo-PATH gap),
//! renders a unit that runs `<bin> serve`, and enables it. System scope needs
//! root; user scope installs a `--user` service. Self-contained: the unit is
//! rendered in-process, not read from a packaging file (works for any adopter).

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::cli::SystemdAction;
use crate::shared::config::Config;
use crate::shared::paths::{ADMIN_GROUP, Scope};

const UNIT_NAME: &str = "ghr-stats.service";

pub fn run(action: SystemdAction, _cfg: &Config) -> Result<()> {
    match action {
        SystemdAction::Install { system, user } => install(resolve_scope(system, user)),
        SystemdAction::Uninstall => uninstall(resolve_scope(false, false)),
    }
}

/// Explicit `--system`/`--user` win; otherwise derive from the effective uid.
/// Shared with `uninstall`, which resolves scope the same way.
pub(crate) fn resolve_scope(system: bool, user: bool) -> Scope {
    match (system, user) {
        (true, _) => Scope::System,
        (_, true) => Scope::User,
        _ => Scope::detect(),
    }
}

fn install(scope: Scope) -> Result<()> {
    // Same requirement as the hook installer: a system unit needs a root
    // *process* (it writes /etc + /usr/local/bin), which per-op sudo cannot
    // provide. `require_root` refuses a non-root process with a re-run hint.
    // (Modelling the file/unit writes as a PrivilegedExecution too is future
    // work — the precedent is in `privileged`.)
    if scope == Scope::System
        && let Err(hint) = crate::shared::privileged::require_root("systemd install --system")
    {
        bail!("system install needs root — re-run `{hint}`");
    }

    let bin = scope.bin_path();
    let src = std::env::current_exe().context("locating the running binary")?;
    copy_bin(&src, &bin)?;

    let unit_path = scope.systemd_unit_path();
    write_file(&unit_path, &render_unit(&bin, scope))?;

    enable(scope)?;

    println!("✓ installed {}", unit_path.display());
    println!("  binary: {}", bin.display());
    println!("  config: {}", scope.config_file().display());
    println!("  data:   {}", scope.data_dir().display());
    println!("  socket: {}", scope.socket_path().display());
    // The admin group only gates the root collector's /etc writes; a user-scope
    // collector owns its config outright, so provision it for System only.
    if scope == Scope::System {
        provision_admin_group();
    }
    Ok(())
}

/// Idempotently create the `ghr-stats` admin group and add the invoking operator
/// (`$SUDO_USER`) to it, so an authorized non-root TUI can edit the root-owned
/// system config over the socket without per-edit sudo. Best-effort: a failure
/// here does NOT fail the install (the service is already up) — it prints the
/// manual command instead. Membership is resolved fresh by the collector on each
/// request, so it takes effect immediately (no re-login).
fn provision_admin_group() {
    // `groupadd -f`: succeeds whether or not the group already exists (idempotent).
    if let Err(e) = run_tool("groupadd", &["-f", ADMIN_GROUP]) {
        println!("  note: could not create the `{ADMIN_GROUP}` group ({e}).");
        println!("        create it + add operators to allow non-root config edits:");
        println!(
            "          sudo groupadd -f {ADMIN_GROUP} && sudo usermod -aG {ADMIN_GROUP} <user>"
        );
        return;
    }
    // Add the human who ran `sudo` (never root itself) to the group.
    match std::env::var("SUDO_USER") {
        Ok(user) if !user.is_empty() && user != "root" => {
            match run_tool("usermod", &["-aG", ADMIN_GROUP, &user]) {
                Ok(()) => println!(
                    "  group:  added {user} to `{ADMIN_GROUP}` — {user} can now edit config \
                     from a non-root TUI (no re-login needed)"
                ),
                Err(e) => {
                    println!("  note: created `{ADMIN_GROUP}` but could not add {user} ({e}).");
                    println!("        run: sudo usermod -aG {ADMIN_GROUP} {user}");
                }
            }
        }
        // Installed directly as root (no SUDO_USER): nothing to add automatically.
        _ => println!(
            "  group:  `{ADMIN_GROUP}` ready — allow a non-root TUI to edit config with: \
             sudo usermod -aG {ADMIN_GROUP} <user>"
        ),
    }
}

/// Run a system administration tool, mapping a non-zero exit / missing binary to
/// an `Err` the caller reports (never propagated — group setup is best-effort).
fn run_tool(tool: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(tool)
        .args(args)
        .status()
        .with_context(|| format!("running {tool}"))?;
    if !status.success() {
        bail!("{tool} {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Disable + remove the service unit (best-effort), leaving data in place. Shared
/// with the top-level `uninstall` orchestrator's `--service` domain.
pub(crate) fn uninstall(scope: Scope) -> Result<()> {
    let unit_path = scope.systemd_unit_path();
    // Best-effort: the unit may already be gone.
    let _ = systemctl(scope, &["disable", "--now", UNIT_NAME]);
    if unit_path.exists() {
        std::fs::remove_file(&unit_path)
            .with_context(|| format!("removing {}", unit_path.display()))?;
    }
    let _ = systemctl(scope, &["daemon-reload"]);
    // The IPC socket lives under the unit's RuntimeDirectory=, which `disable
    // --now` already tears down; remove it defensively for a foreground/dev
    // collector or an already-stopped unit (tmpfs, so usually a no-op).
    let _ = std::fs::remove_file(scope.socket_path());
    let _ = std::fs::remove_dir(scope.runtime_dir());
    println!(
        "✓ removed {} (data left in {})",
        unit_path.display(),
        scope.data_dir().display()
    );
    Ok(())
}

/// Render the systemd unit. Pure (no I/O) so it is unit-tested.
fn render_unit(bin: &Path, scope: Scope) -> String {
    let wanted_by = match scope {
        Scope::System => "multi-user.target",
        Scope::User => "default.target",
    };
    // RuntimeDirectory= makes systemd create + own /run/ghr-stats (and remove it
    // on stop), so the IPC socket lives on tmpfs with no stale-file cleanup to do.
    // Mode 0755 keeps the dir world-traversable so a non-root TUI can reach a
    // root service's socket (the socket itself is widened to 0666 by the server).
    format!(
        "[Unit]\n\
         Description=ghr-stats collector — sample self-hosted runners, serve metrics + TUI IPC\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} serve\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         RuntimeDirectory=ghr-stats\n\
         RuntimeDirectoryMode=0755\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        bin = bin.display(),
    )
}

fn copy_bin(src: &Path, dst: &Path) -> Result<()> {
    if src == dst {
        return Ok(()); // re-install from the installed path: nothing to copy
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to a sibling temp file, then atomically rename over `dst`. A plain
    // copy overwrites `dst` IN PLACE and fails with ETXTBSY ("text file busy")
    // when `dst` is the currently-running collector — i.e. every upgrade of a
    // live service. rename swaps the inode instead: the running process keeps
    // executing the old (now-unlinked) file, and new starts pick up the new one.
    // Same directory ⇒ same filesystem ⇒ the rename is atomic.
    let tmp = dst.with_extension("new");
    std::fs::copy(src, &tmp)
        .with_context(|| format!("staging {} → {}", src.display(), tmp.display()))?;
    std::fs::rename(&tmp, dst)
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp); // don't leave a stray .new on failure
        })
        .with_context(|| format!("installing {} → {}", src.display(), dst.display()))?;
    Ok(())
}

fn enable(scope: Scope) -> Result<()> {
    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", UNIT_NAME])?;
    // `restart` (not `start`/`--now`): on a fresh install it just starts, but on
    // a re-install over an already-active service it swaps in the new binary +
    // unit — `start` alone would no-op and leave the old process running.
    systemctl(scope, &["restart", UNIT_NAME])?;
    Ok(())
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let mut cmd = std::process::Command::new("systemctl");
    if scope == Scope::User {
        cmd.arg("--user");
    }
    cmd.args(args);
    let status = cmd.status().context("running systemctl")?;
    if !status.success() {
        bail!("systemctl {} failed ({status})", args.join(" "));
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_unit_runs_serve_and_wants_multi_user() {
        let u = render_unit(Path::new("/usr/local/bin/ghr-stats"), Scope::System);
        assert!(u.contains("ExecStart=/usr/local/bin/ghr-stats serve"));
        assert!(u.contains("WantedBy=multi-user.target"));
        assert!(u.contains("Type=simple"));
        // The IPC socket lives under the systemd-managed RuntimeDirectory.
        assert!(u.contains("RuntimeDirectory=ghr-stats"));
        assert!(u.contains("RuntimeDirectoryMode=0755"));
    }

    #[test]
    fn user_unit_wants_default_target() {
        let u = render_unit(Path::new("/home/x/.local/bin/ghr-stats"), Scope::User);
        assert!(u.contains("ExecStart=/home/x/.local/bin/ghr-stats serve"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("RuntimeDirectory=ghr-stats"));
    }

    #[test]
    fn copy_bin_creates_parent_and_replaces_via_rename() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-bin");
        let dst = dir.path().join("sub/dst-bin"); // parent must be created
        std::fs::write(&src, b"v1").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();

        copy_bin(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v1");
        // Executable bit survives the copy.
        assert_ne!(
            std::fs::metadata(&dst).unwrap().permissions().mode() & 0o111,
            0
        );

        // Re-install (the upgrade case ETXTBSY breaks with a plain in-place copy):
        // a second call replaces the target via rename and leaves no stray `.new`.
        std::fs::write(&src, b"v2-longer").unwrap();
        copy_bin(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"v2-longer");
        assert!(!dst.with_extension("new").exists());
    }
}
