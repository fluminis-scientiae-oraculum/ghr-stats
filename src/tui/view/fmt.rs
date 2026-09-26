//! Value → display token. Absence renders as `—`, never `0`: no reading is not a zero reading.

use crate::shared::hooks::install::HookStatus;
pub(crate) use crate::shared::util::fmt_bytes;
use ratatui::style::Color;

use crate::shared::models::Liveness;
use crate::shared::util::now_epoch;

/// Runner names share long prefixes, so trimming the tail would make them identical.
pub(crate) fn ellipsize_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep / 2;
    let h: String = chars[..head].iter().collect();
    let t: String = chars[chars.len() - tail..].iter().collect();
    format!("{h}…{t}")
}

pub(crate) fn fmt_opt_bytes(bytes: Option<u64>) -> String {
    bytes.map(fmt_bytes).unwrap_or_else(|| "—".to_string())
}

pub(crate) fn fmt_cpu(pct: Option<f32>) -> String {
    pct.map(|v| format!("{v:.1}%"))
        .unwrap_or_else(|| "—".to_string())
}

pub(crate) fn fmt_uptime(secs: Option<u64>) -> String {
    let Some(s) = secs else {
        return "—".to_string();
    };
    let (d, h, m) = (s / 86_400, (s % 86_400) / 3_600, (s % 3_600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

pub(crate) fn fmt_ago(ts: Option<i64>) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
    let d = (now_epoch() - ts).max(0);
    if d < 60 {
        format!("{d}s ago")
    } else if d < 3_600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3_600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

pub(crate) fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

pub(crate) fn liveness_label(l: Liveness) -> (&'static str, Color) {
    match l {
        Liveness::Busy => ("● busy", Color::Green),
        Liveness::Idle => ("○ idle", Color::Cyan),
        Liveness::Offline => ("× offline", Color::Red),
    }
}

/// Axis labels want round granularity, unlike [`fmt_dur`]'s m+s precision.
pub(super) fn rel_label(ts: i64, now: i64) -> String {
    let age = (now - ts).max(0) as u64;
    match age {
        0 => "now".to_string(),
        s if s < 60 => format!("-{s}s"),
        s if s < 3_600 => format!("-{}m", s / 60),
        s => format!("-{}h{}m", s / 3_600, (s % 3_600) / 60),
    }
}

pub(crate) fn hook_glyph(h: HookStatus) -> &'static str {
    match h {
        HookStatus::Ours => "✓",
        HookStatus::Foreign | HookStatus::Unset => "✗",
        HookStatus::Unreadable => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_opt_bytes(None), "—");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(fmt_dur(5), "5s");
        assert_eq!(fmt_dur(59), "59s");
        assert_eq!(fmt_dur(90), "1m30s");
        assert_eq!(fmt_dur(3661), "61m1s");
    }

    #[test]
    fn uptime_and_cpu_formatting() {
        assert_eq!(fmt_uptime(Some(0)), "0m");
        assert_eq!(fmt_uptime(Some(3_660)), "1h1m");
        assert_eq!(fmt_uptime(Some(172_800)), "2d0h");
        assert_eq!(fmt_uptime(None), "—");
        assert_eq!(fmt_cpu(Some(12.34)), "12.3%");
        assert_eq!(fmt_cpu(None), "—");
    }

    #[test]
    fn relative_axis_labels() {
        let now = 10_000;
        assert_eq!(rel_label(now, now), "now");
        assert_eq!(rel_label(now - 45, now), "-45s");
        assert_eq!(rel_label(now - 90, now), "-1m");
        assert_eq!(rel_label(now - (5 * 3600 + 9 * 60), now), "-5h9m");
        assert_eq!(rel_label(now + 5, now), "now");
    }

    #[test]
    fn ellipsize_keeps_short_strings_and_trims_long_ones() {
        assert_eq!(ellipsize_middle("hello", 10), "hello");
        let e = ellipsize_middle("self-hosted-runner-01", 10);
        assert_eq!(e.chars().count(), 10);
        assert!(e.contains('…'));
        assert!(e.starts_with("self-"));
        assert!(e.ends_with("r-01"));
    }
}
