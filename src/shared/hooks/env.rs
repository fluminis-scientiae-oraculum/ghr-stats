//! Privileged writes to a runner's own `.env` — shared by the config wizard
//! (which *wires* the hook vars) and `uninstall` (which *reverts* them).
//!
//! A runner's `.env` is owned by the runner user and lives on a root-owned
//! install dir, so both writing and reverting go through `privileged::run` with
//! `install(1)` to preserve ownership + mode. Keeping this in one place means the
//! two directions can never drift on ownership/mode (they must stay symmetric).

use std::path::Path;

use crate::shared::privileged::{self, Outcome};

/// Install `content` as `env_path`, owned by `user`, mode `0644` — via the
/// privileged path (direct when root, else `sudo`). Returns the [`Outcome`] so
/// each caller renders its own message. The staging temp file is always removed.
pub(crate) fn write_env_as_root(env_path: &Path, content: &str, user: &str) -> Outcome {
    let tmp = std::env::temp_dir().join(format!("ghr-env-{}", std::process::id()));
    if std::fs::write(&tmp, content).is_err() {
        return Outcome::Failed {
            code: None,
            stderr: "could not stage .env update".to_string(),
        };
    }
    let (tmp_s, env_s) = (tmp.to_string_lossy(), env_path.to_string_lossy());
    let outcome = privileged::run(&[
        "install", "-o", user, "-g", user, "-m", "0644", &tmp_s, &env_s,
    ]);
    let _ = std::fs::remove_file(&tmp);
    outcome
}
