//! Small shared helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Crate version; distinct from the IPC wire [`crate::shared::ipc::VERSION`].
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC-3339 UTC with whole seconds (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn to_rfc3339_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's `civil_from_days`.
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

pub(crate) fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1_572_864), "1.5 MiB");
    }

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(to_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(to_rfc3339_utc(1_784_998_498), "2026-07-25T16:54:58Z");
        // Leap day, and the century rule (2000 is a leap year, 1900 is not).
        assert_eq!(to_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(to_rfc3339_utc(1_735_689_599), "2024-12-31T23:59:59Z");
        assert_eq!(to_rfc3339_utc(1_735_689_600), "2025-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_handles_pre_epoch_without_panicking() {
        assert_eq!(to_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
    }
}
