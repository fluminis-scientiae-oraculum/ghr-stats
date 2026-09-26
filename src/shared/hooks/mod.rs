//! Runner job-event hooks: `ingest` tails the NDJSON log the hooks write; `install` manages
//! the hook scripts.

use std::path::{Path, PathBuf};

pub mod env;
pub mod ingest;
pub mod install;

/// Per-runner event log in the install-dir root: the runner user owns it, so the hook can always
/// append and root can read it. Never under `_work`, which job checkouts wipe.
pub const RUNNER_EVENT_LOG: &str = ".ghr-stats-events.ndjson";

/// The installer's `.env` `GHR_STATS_EVENT_LOG` and the collector's tail both derive the path here.
pub fn runner_event_log(dir: &Path) -> PathBuf {
    dir.join(RUNNER_EVENT_LOG)
}
