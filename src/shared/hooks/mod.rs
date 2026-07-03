//! The runner job-event hook boundary.
//!
//! `ingest` tails the append-only NDJSON log the hooks write; `install`
//! (detect / chain / instruct, P6) manages the hook scripts. Kept separate
//! from `collectors` on purpose: these read and manage the runner-hook
//! contract, they do not sample host resources.

use std::path::{Path, PathBuf};

pub mod env;
pub mod ingest;
pub mod install;
pub mod uninstall;

/// Filename of a runner's own append-only job-event log. It lives in the runner's
/// install-dir root — which the runner *user* owns — so the hook (running as that
/// user) can always create and append to it, and the root collector can always
/// read it. This sidesteps the shared-writable-log permission problem entirely:
/// no shared file, no group juggling, no chmod of a root-owned dir. A dotfile in
/// the install-dir root (never under `_work`, which a job checkout wipes).
pub const RUNNER_EVENT_LOG: &str = ".ghr-stats-events.ndjson";

/// The per-runner event-log path for a runner installed at `dir`.
///
/// SINGLE SOURCE OF TRUTH for the hook contract: the installer points each
/// runner's `.env` `GHR_STATS_EVENT_LOG` at this path (`install` / `wizard`), and
/// the collector derives the very same path to tail (`service::serve`). Because
/// both sides call this one function they can never drift onto different paths —
/// the class of bug where the writer and reader disagree becomes unrepresentable.
pub fn runner_event_log(dir: &Path) -> PathBuf {
    dir.join(RUNNER_EVENT_LOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_event_log_is_a_dotfile_in_the_install_dir() {
        let dir = Path::new("/srv/actions-runner/runner-01");
        let log = runner_event_log(dir);
        // In the runner's own dir (the runner user owns it → always writable),
        // and a dotfile in the *root* (not under `_work`, which checkouts wipe).
        assert_eq!(log, dir.join(".ghr-stats-events.ndjson"));
        assert!(log.starts_with(dir));
        assert!(!log.to_string_lossy().contains("_work"));
    }
}
