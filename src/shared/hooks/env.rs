//! A runner's `.env`: reading it safely, rewriting it with the ownership and mode
//! it already had, and restarting the runner so the change takes effect.

use std::io;
use std::path::{Path, PathBuf};

use crate::shared::collectors::runners;
use crate::shared::models::Liveness;
use crate::shared::privileged::{self, Outcome, PrivilegedCall, UnitVerb};
use crate::shared::runner_files::{self, Ownership};

const ENV_CAP: u64 = 1024 * 1024;

/// A runner's `.env` as read, with the ownership a rewrite must keep.
#[derive(PartialEq, Eq)]
pub(crate) struct EnvFile {
    pub dir: PathBuf,
    pub text: String,
    pub ownership: Ownership,
}

impl EnvFile {
    /// `Err` when the file no longer matches this read, so a rewrite would discard an edit.
    fn check_unchanged(&self) -> Result<(), String> {
        match read(&self.dir) {
            Ok(now) if now == *self => Ok(()),
            Ok(_) => Err("changed since it was read; left as is".to_string()),
            Err(e) => Err(format!("unreadable now ({e}); left as is")),
        }
    }
}

/// Read `dir/.env`. A missing file reads as empty, owned like the install dir.
pub(crate) fn read(dir: &Path) -> io::Result<EnvFile> {
    let (text, ownership) = match runner_files::read_text(dir, ".env", ENV_CAP) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            (String::new(), Ownership::default_in(dir)?)
        }
        Err(e) => return Err(e),
    };
    Ok(EnvFile {
        dir: dir.to_path_buf(),
        text,
        ownership,
    })
}

/// Replace `env` with `content` via the privileged path, only while the file still
/// matches `env`. The staging file is a `NamedTempFile` (`O_EXCL`, random name), so no
/// pre-planted symlink can redirect the root write.
pub(crate) fn write_env_as_root(env: &EnvFile, content: &str) -> Outcome {
    use std::io::Write;
    // Accepted: an edit landing between this check and `install` is still overwritten.
    if let Err(why) = env.check_unchanged() {
        return Outcome::Failed {
            code: None,
            stderr: why,
        };
    }
    let mut tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(_) => return stage_failed(),
    };
    if tmp.write_all(content.as_bytes()).is_err() {
        return stage_failed();
    }
    privileged::run(&PrivilegedCall::InstallEnvFile {
        src: tmp.path().to_path_buf(),
        dst: env.dir.join(".env"),
        ownership: env.ownership,
    })
}

fn stage_failed() -> Outcome {
    Outcome::Failed {
        code: None,
        stderr: "could not stage .env update".to_string(),
    }
}

/// Restart the runner at `dir` so a rewritten `.env` takes effect, unless it is
/// running a job or not running at all. Returns a suffix for the receipt line.
pub(crate) fn restart_if_idle(dir: &Path) -> String {
    let unit = match runners::unit_for(dir) {
        Ok(unit) => unit,
        Err(why) => return format!(" (not restarted: {why})"),
    };
    match runners::liveness_in(dir, &crate::shared::collectors::procscan::scan()) {
        Liveness::Busy => " (busy, applies on its next restart)".to_string(),
        Liveness::Offline => " (not running, applies when it starts)".to_string(),
        Liveness::Idle => {
            let o = privileged::run(&PrivilegedCall::Systemctl {
                verb: UnitVerb::Restart,
                unit: unit.clone(),
            });
            if o.is_ok() {
                format!(" (restarted {unit})")
            } else {
                format!(" ({})", o.describe(&format!("restart {unit}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_goes_stale_when_the_file_is_edited_created_or_its_mode_changes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");

        let missing = read(dir.path()).unwrap();
        assert_eq!(missing.check_unchanged(), Ok(()));
        std::fs::write(&path, "A=1\n").unwrap();
        assert!(missing.check_unchanged().is_err());

        let present = read(dir.path()).unwrap();
        assert_eq!(present.check_unchanged(), Ok(()));
        std::fs::write(&path, "A=2\n").unwrap();
        assert!(present.check_unchanged().is_err());

        let edited = read(dir.path()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(edited.check_unchanged().is_err());
    }
}
