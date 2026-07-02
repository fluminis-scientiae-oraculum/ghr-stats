//! The single owner of where ghr-stats reads and writes on disk.
//!
//! Path policy lives here and nowhere else. A privilege [`Scope`] — derived
//! from the effective uid, or forced for `systemd install` — maps to a config
//! directory and a data directory. Every other module asks `paths` rather than
//! hand-rolling `/etc` vs XDG; that separation is the whole reason this module
//! exists. A system deployment (root `serve` service + `sudo ghr-stats` TUI)
//! keeps everything under `/etc` + `/var/lib`; a personal/dev deployment is
//! all-user under the XDG base dirs. **Config is the exception: it is always the
//! canonical system file at `/etc/ghr-stats/config.toml`** ([`resolve_config`] /
//! [`config_write_target`]) — one root-owned source of truth for the collector,
//! never duplicated per-user (secrets live there once). Only the data / runtime
//! / unit / binary paths are scope-derived. Data dirs are never mixed across
//! scopes (plan §2/S9) — the one deliberate exception is the TUI's IPC client,
//! which may `connect` to another scope's collector socket to read (only)
//! derived fleet stats, so a non-root dashboard can observe a root system
//! service without ever touching its `/var/lib` database.

use std::path::{Path, PathBuf};

/// The Unix group whose members (plus root) may mutate the root-owned system
/// config over the IPC socket. `systemd install` provisions it and adds the
/// operator; the collector's peer-cred authz ([`crate::service::ipc_server`])
/// checks it. One literal, so the installer and the authz gate never drift.
pub(crate) const ADMIN_GROUP: &str = "ghr-stats";

/// Which on-disk layout a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Per-user: `$XDG_CONFIG_HOME` + `$XDG_DATA_HOME`.
    User,
    /// System-wide: `/etc/ghr-stats` + `/var/lib/ghr-stats`.
    System,
}

impl Scope {
    /// Pick the layout from the effective uid: root ⇒ [`Scope::System`].
    pub fn detect() -> Self {
        if uzers::get_effective_uid() == 0 {
            Scope::System
        } else {
            Scope::User
        }
    }

    /// The scope's short name for display ("system" / "user"). The single home
    /// for this mapping (was duplicated as `scope_word`/`scope_label`).
    pub fn label(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }

    /// Directory holding `config.toml`.
    pub fn config_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/etc/ghr-stats"),
            Scope::User => xdg_config_dir().join("ghr-stats"),
        }
    }

    /// Directory holding the database + event log.
    pub fn data_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/var/lib/ghr-stats"),
            Scope::User => xdg_data_dir().join("ghr-stats"),
        }
    }

    /// The scope's `config.toml`.
    pub fn config_file(self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// The scope's SQLite database.
    pub fn db_path(self) -> PathBuf {
        self.data_dir().join("ghr-stats.db")
    }

    /// The scope's append-only job-event log.
    pub fn event_log(self) -> PathBuf {
        self.data_dir().join("events.ndjson")
    }

    /// Absolute install path for the binary, so a `systemd install` unit and a
    /// later `sudo ghr-stats` resolve the same file (the sudo-PATH gap, #8).
    pub fn bin_path(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/usr/local/bin/ghr-stats"),
            Scope::User => home().join(".local/bin/ghr-stats"),
        }
    }

    /// Where the systemd unit file is written.
    pub fn systemd_unit_path(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/etc/systemd/system/ghr-stats.service"),
            Scope::User => xdg_config_dir().join("systemd/user/ghr-stats.service"),
        }
    }

    /// The runtime directory holding the collector's IPC socket. System uses
    /// `/run/ghr-stats` (created by the unit's `RuntimeDirectory=`, world-
    /// traversable so a non-root TUI can reach a root service's socket); User
    /// uses `$XDG_RUNTIME_DIR/ghr-stats` (falling back to `/run/user/<uid>`).
    /// On tmpfs, so a stale socket never outlives a reboot.
    pub fn runtime_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/run/ghr-stats"),
            Scope::User => xdg_runtime_dir().join("ghr-stats"),
        }
    }

    /// The collector's IPC socket — the TUI's Persistent-mode data channel and
    /// its liveness probe (a successful `connect` ⇒ Persistent).
    pub fn socket_path(self) -> PathBuf {
        self.runtime_dir().join("serve.sock")
    }
}

/// An explicitly-pointed config target, shared by load-resolution AND the
/// write-target so the two can never diverge: an explicit `--config`, then
/// `$GHR_STATS_CONFIG`. `None` ⇒ neither was given, so both fall back to the
/// canonical system path. Keeping this in one place is the whole point — a
/// config edit must land back in the file it was loaded from.
fn explicit_config_target(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    std::env::var_os("GHR_STATS_CONFIG").map(PathBuf::from)
}

/// The single canonical config location: the **system** file at
/// `/etc/ghr-stats/config.toml`. The config holds secrets (PATs) + the fleet
/// roots and is the collector's source of truth, so it is one root-owned artifact
/// — never duplicated per-user (that only invites secret sprawl + drift). An
/// explicit `--config`/`$GHR_STATS_CONFIG` still overrides it (tests, non-default
/// deployments).
pub fn resolve_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit_config_target(explicit) {
        return Some(p);
    }
    let system = Scope::System.config_file();
    system.exists().then_some(system)
}

/// Where `ghr-stats config` and the TUI's config edits WRITE the config: the same
/// canonical `/etc` file [`resolve_config`] reads (or the explicit override).
/// Writing `/etc` needs root — the system-deployment model (`sudo ghr-stats`)
/// already has it; a non-root write fails with a "re-run with sudo" error.
pub fn config_write_target(explicit: Option<&Path>) -> PathBuf {
    explicit_config_target(explicit).unwrap_or_else(|| Scope::System.config_file())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
}

fn xdg_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
}

/// `$XDG_RUNTIME_DIR`, falling back to the systemd-conventional `/run/user/<uid>`
/// when it is unset (e.g. a bare login shell without a user session bus).
fn xdg_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uzers::get_effective_uid())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_scope_uses_etc_and_var_lib() {
        assert_eq!(
            Scope::System.config_file(),
            PathBuf::from("/etc/ghr-stats/config.toml")
        );
        assert_eq!(
            Scope::System.db_path(),
            PathBuf::from("/var/lib/ghr-stats/ghr-stats.db")
        );
        assert_eq!(
            Scope::System.event_log(),
            PathBuf::from("/var/lib/ghr-stats/events.ndjson")
        );
        assert_eq!(
            Scope::System.bin_path(),
            PathBuf::from("/usr/local/bin/ghr-stats")
        );
    }

    #[test]
    fn system_ipc_socket_lives_on_run_tmpfs() {
        // Deterministic (no env): the system collector's socket is under /run,
        // matching the unit's `RuntimeDirectory=ghr-stats`.
        assert_eq!(Scope::System.runtime_dir(), PathBuf::from("/run/ghr-stats"));
        assert_eq!(
            Scope::System.socket_path(),
            PathBuf::from("/run/ghr-stats/serve.sock")
        );
    }

    #[test]
    fn explicit_config_target_is_shared_by_load_and_write() {
        // Alignment guarantee: an explicitly-pointed config is BOTH loaded from
        // and written back to the same file. `resolve_config` and
        // `config_write_target` must agree on the shared tiers — previously
        // `$GHR_STATS_CONFIG` was honored on load but ignored on write, so an
        // edit silently landed in a different file. Explicit `--config` exercises
        // the shared `explicit_config_target` path without touching process env
        // or euid, so this stays deterministic under the parallel test runner.
        let p = Path::new("/etc/ghr-stats/pinned.toml");
        assert_eq!(explicit_config_target(Some(p)), Some(p.to_path_buf()));
        assert_eq!(resolve_config(Some(p)), Some(p.to_path_buf()));
        // Explicit wins over the canonical `/etc` default in write-target.
        assert_eq!(config_write_target(Some(p)), p.to_path_buf());
    }

    #[test]
    fn detect_matches_effective_uid() {
        let expected = if uzers::get_effective_uid() == 0 {
            Scope::System
        } else {
            Scope::User
        };
        assert_eq!(Scope::detect(), expected);
    }
}
