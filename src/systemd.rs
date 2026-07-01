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
use crate::config::Config;
use crate::paths::Scope;

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
        && let Err(hint) = crate::privileged::require_root("systemd install --system")
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
    format!(
        "[Unit]\n\
         Description=ghr-stats — sample self-hosted runners + expose metrics\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin} serve\n\
         Restart=on-failure\n\
         RestartSec=5\n\
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
    std::fs::copy(src, dst)
        .with_context(|| format!("copying {} → {}", src.display(), dst.display()))?;
    Ok(())
}

fn enable(scope: Scope) -> Result<()> {
    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", "--now", UNIT_NAME])?;
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
    }

    #[test]
    fn user_unit_wants_default_target() {
        let u = render_unit(Path::new("/home/x/.local/bin/ghr-stats"), Scope::User);
        assert!(u.contains("ExecStart=/home/x/.local/bin/ghr-stats serve"));
        assert!(u.contains("WantedBy=default.target"));
    }
}
