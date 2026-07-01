//! The single owner of where ghr-stats reads and writes on disk.
//!
//! Path policy lives here and nowhere else. A privilege [`Scope`] — derived
//! from the effective uid, or forced for `systemd install` — maps to a config
//! directory and a data directory. Every other module asks `paths` rather than
//! hand-rolling `/etc` vs XDG; that separation is the whole reason this module
//! exists. A system deployment (root `serve` service + `sudo ghr-stats` TUI)
//! keeps everything under `/etc` + `/var/lib`; a personal/dev deployment is
//! all-user under the XDG base dirs. Config + data dirs are never mixed across
//! scopes (plan §2/S9) — the one deliberate exception is the TUI's IPC client,
//! which may `connect` to another scope's collector socket to read (only)
//! derived fleet stats, so a non-root dashboard can observe a root system
//! service without ever touching its `/var/lib` database.

use std::path::{Path, PathBuf};

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
/// write-target so the two can never diverge on the tiers they have in common:
/// an explicit `--config`, then `$GHR_STATS_CONFIG`. `None` ⇒ neither was given,
/// so the caller falls back to its own policy (load: the scope file if it exists;
/// write: the sudo-invoker's home, else the scope file). Keeping this in one
/// place is the whole point — a config edit must land back in the file it was
/// loaded from (previously `$GHR_STATS_CONFIG` was honored on load but ignored on
/// write, so edits silently went to a different file).
fn explicit_config_target(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    std::env::var_os("GHR_STATS_CONFIG").map(PathBuf::from)
}

/// Resolve which config file to load, if any.
///
/// Order: explicit `--config` → `$GHR_STATS_CONFIG` → the scope's `config.toml`
/// if it exists. `None` ⇒ run on built-in defaults.
pub fn resolve_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit_config_target(explicit) {
        return Some(p);
    }
    let scoped = Scope::detect().config_file();
    scoped.exists().then_some(scoped)
}

/// The user a root process is really acting for: `$SUDO_USER` when invoked via
/// sudo, else `None`. `ghr-stats config` is run as root only to reach a
/// privileged step (hook install); the CONFIG it writes is user-facing and must
/// land where that user's own non-root TUI reads it.
fn sudo_invoker() -> Option<uzers::User> {
    let name = std::env::var_os("SUDO_USER")?;
    if name.is_empty() {
        return None;
    }
    uzers::get_user_by_name(&name)
}

/// Where `ghr-stats config` and the TUI's config edits WRITE the config file.
///
/// Order: explicit `--config` → `$GHR_STATS_CONFIG` → (under sudo) the invoking
/// user's `~/.config/ghr-stats/config.toml` (so their non-root TUI reads it —
/// NOT root's `/etc`, which a non-root process can't read) → the current scope's
/// file. The first two tiers are shared verbatim with [`resolve_config`] via
/// [`explicit_config_target`], so a config loaded from `$GHR_STATS_CONFIG` is
/// also WRITTEN there (edits round-trip to the same file). Pair with
/// [`chown_to_invoker`] so a `0600` file written by root stays readable by that
/// user. This is what lets `sudo ghr-stats config` (sudo only for hooks) coexist
/// with a plain `ghr-stats` TUI.
pub fn config_write_target(explicit: Option<&Path>) -> PathBuf {
    use uzers::os::unix::UserExt;
    if let Some(p) = explicit_config_target(explicit) {
        return p;
    }
    if let Some(u) = sudo_invoker() {
        return u.home_dir().join(".config/ghr-stats/config.toml");
    }
    Scope::detect().config_file()
}

/// Hand a just-written user-facing file (and the dir we created for it) back to
/// the sudo invoker, so their non-root processes can read a `0600` config.
/// No-op when not under sudo or the user is unknown; best-effort.
pub fn chown_to_invoker(path: &Path) {
    let Some(u) = sudo_invoker() else {
        return;
    };
    let uid = Some(nix::unistd::Uid::from_raw(u.uid()));
    let gid = Some(nix::unistd::Gid::from_raw(u.primary_group_id()));
    if let Some(parent) = path.parent() {
        let _ = nix::unistd::chown(parent, uid, gid);
    }
    let _ = nix::unistd::chown(path, uid, gid);
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
        // Explicit wins over the sudo-invoker / scope fallback in write-target.
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
