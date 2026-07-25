//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// This build's crate version — the ONE place `CARGO_PKG_VERSION` is read.
///
/// Distinct from [`crate::shared::ipc::VERSION`], which is the IPC wire
/// protocol number: two binaries can share a wire version across several
/// releases, and a wire bump does not imply a release. Both are shown in the
/// TUI precisely because their disagreement is diagnostic.
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current Unix time in whole seconds (0 if the clock predates the epoch).
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
