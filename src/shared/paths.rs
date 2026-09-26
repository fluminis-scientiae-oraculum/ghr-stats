//! On-disk layout per [`Scope`] (from the effective uid, or forced for `systemd install`).

use std::path::{Path, PathBuf};

/// Unix group whose members (plus root) may mutate the system config over IPC.
pub(crate) const ADMIN_GROUP: &str = "ghr-stats";

/// Which on-disk layout a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    pub fn detect() -> Self {
        if uzers::get_effective_uid() == 0 {
            Scope::System
        } else {
            Scope::User
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }

    pub fn config_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/etc/ghr-stats"),
            Scope::User => xdg_config_dir().join("ghr-stats"),
        }
    }

    pub fn data_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/var/lib/ghr-stats"),
            Scope::User => xdg_data_dir().join("ghr-stats"),
        }
    }

    pub fn config_file(self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    pub fn db_path(self) -> PathBuf {
        self.data_dir().join("ghr-stats.db")
    }

    /// Append-only job-event log.
    pub fn event_log(self) -> PathBuf {
        self.data_dir().join("events.ndjson")
    }

    /// Fixed path so the unit and a later `sudo ghr-stats` (sudo's `secure_path`) resolve
    /// the same binary.
    pub fn bin_path(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/usr/local/bin/ghr-stats"),
            Scope::User => home().join(".local/bin/ghr-stats"),
        }
    }

    pub fn systemd_unit_path(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/etc/systemd/system/ghr-stats.service"),
            Scope::User => xdg_config_dir().join("systemd/user/ghr-stats.service"),
        }
    }

    /// Holds the IPC socket. On tmpfs, so a stale socket never outlives a reboot;
    /// the System dir is created by the unit's `RuntimeDirectory=`.
    pub fn runtime_dir(self) -> PathBuf {
        match self {
            Scope::System => PathBuf::from("/run/ghr-stats"),
            Scope::User => xdg_runtime_dir().join("ghr-stats"),
        }
    }

    pub fn socket_path(self) -> PathBuf {
        self.runtime_dir().join("serve.sock")
    }
}

/// `--config`, then `$GHR_STATS_CONFIG`. Shared by load and write so an edit lands
/// in the file it was loaded from.
fn explicit_config_target(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    std::env::var_os("GHR_STATS_CONFIG").map(PathBuf::from)
}

/// Config is never per-user: it holds PATs, so it is the root-owned system file
/// unless explicitly overridden.
pub fn resolve_config(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit_config_target(explicit) {
        return Some(p);
    }
    let system = Scope::System.config_file();
    system.exists().then_some(system)
}

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

fn xdg_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uzers::get_effective_uid())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_config_target_is_shared_by_load_and_write() {
        let p = Path::new("/etc/ghr-stats/pinned.toml");
        assert_eq!(explicit_config_target(Some(p)), Some(p.to_path_buf()));
        assert_eq!(resolve_config(Some(p)), Some(p.to_path_buf()));
        assert_eq!(config_write_target(Some(p)), p.to_path_buf());
    }
}
