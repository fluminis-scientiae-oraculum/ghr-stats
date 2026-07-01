//! The single owner of where ghr-stats reads and writes on disk.
//!
//! Path policy lives here and nowhere else. A privilege [`Scope`] — derived
//! from the effective uid, or forced for `systemd install` — maps to a config
//! directory and a data directory. Every other module asks `paths` rather than
//! hand-rolling `/etc` vs XDG; that separation is the whole reason this module
//! exists. A system deployment (root `serve` service + `sudo ghr-stats` TUI)
//! keeps everything under `/etc` + `/var/lib`; a personal/dev deployment is
//! all-user under the XDG base dirs. The two are not mixed (plan §2/S9).

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
}

/// Resolve which config file to load, if any.
///
/// Order: explicit `--config` → `$GHR_STATS_CONFIG` → the scope's `config.toml`
/// if it exists. `None` ⇒ run on built-in defaults.
pub fn resolve_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("GHR_STATS_CONFIG") {
        return Some(PathBuf::from(p));
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
/// Explicit `--config` wins; else, when running under sudo, the invoking user's
/// `~/.config/ghr-stats/config.toml` (so their non-root TUI reads it — NOT
/// root's `/etc`, which a non-root process can't read); else the current scope's
/// file. Pair with [`chown_to_invoker`] so a `0600` file written by root stays
/// readable by that user. This is what lets `sudo ghr-stats config` (sudo only
/// for hooks) coexist with a plain `ghr-stats` TUI.
pub fn config_write_target(explicit: Option<&Path>) -> PathBuf {
    use uzers::os::unix::UserExt;
    if let Some(p) = explicit {
        return p.to_path_buf();
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
    fn detect_matches_effective_uid() {
        let expected = if uzers::get_effective_uid() == 0 {
            Scope::System
        } else {
            Scope::User
        };
        assert_eq!(Scope::detect(), expected);
    }
}
