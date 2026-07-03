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
/// each caller renders its own message.
///
/// SECURITY: this runs as root, so the staging file must not be a predictable
/// path in a shared directory — a local user could pre-plant a symlink there and
/// redirect the root write (CWE-59/CWE-377). `NamedTempFile` creates the staging
/// file with `O_CREAT|O_EXCL` and a random name, defeating that. It is removed on
/// drop (including every early return).
pub(crate) fn write_env_as_root(env_path: &Path, content: &str, user: &str) -> Outcome {
    use std::io::Write;
    let mut tmp = match tempfile::NamedTempFile::new() {
        Ok(t) => t,
        Err(_) => return stage_failed(),
    };
    if tmp.write_all(content.as_bytes()).is_err() {
        return stage_failed();
    }
    let (tmp_s, env_s) = (tmp.path().to_string_lossy(), env_path.to_string_lossy());
    privileged::run(&[
        "install", "-o", user, "-g", user, "-m", "0644", &tmp_s, &env_s,
    ])
    // `tmp` drops here, unlinking the staging file.
}

fn stage_failed() -> Outcome {
    Outcome::Failed {
        code: None,
        stderr: "could not stage .env update".to_string(),
    }
}
