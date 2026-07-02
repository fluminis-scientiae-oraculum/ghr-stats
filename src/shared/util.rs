//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in whole seconds (0 if the clock predates the epoch).
pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
