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

/// Format a Unix timestamp as an RFC-3339 / ISO-8601 UTC instant
/// (`2026-07-25T16:54:58Z`).
///
/// Hand-rolled rather than pulling in a date crate, following the same call this
/// codebase already made for the Prometheus exposition: the need is one
/// fixed-format UTC rendering, and the civil-from-days conversion below is exact
/// integer arithmetic (Howard Hinnant's algorithm, valid across the proleptic
/// Gregorian calendar). No timezones, no locales, no leap seconds — which is
/// precisely what a machine-facing payload wants.
pub fn to_rfc3339_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 → (year, month, day). Exact integer arithmetic; the
/// era-based form handles leap years and centuries without a lookup table.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(to_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // The incident this release is about.
        assert_eq!(to_rfc3339_utc(1_784_998_498), "2026-07-25T16:54:58Z");
        // Leap day, and the century rule (2000 is a leap year, 1900 is not).
        assert_eq!(to_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        // End-of-year rollover.
        assert_eq!(to_rfc3339_utc(1_735_689_599), "2024-12-31T23:59:59Z");
        assert_eq!(to_rfc3339_utc(1_735_689_600), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_handles_pre_epoch_without_panicking() {
        // Euclidean division, not truncating — a negative epoch must not wrap
        // into a bogus time-of-day.
        assert_eq!(to_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
    }
}
