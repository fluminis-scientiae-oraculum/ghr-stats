//! Rendering. One module per view; shared formatting helpers live here.

mod jobs;
mod overview;
mod runner;
mod trends;

use ratatui::Frame;
use ratatui::style::Color;

use crate::model::Liveness;
use crate::tui::app::{App, View};
use crate::util::now_epoch;

pub(crate) fn draw(f: &mut Frame, app: &App) {
    match app.view {
        View::Overview => overview::draw(f, app),
        View::Detail => runner::draw(f, app),
        View::Trends => trends::draw(f, app),
        View::Jobs => jobs::draw(f, app),
    }
}

/// Human-readable byte size (binary units).
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

/// Relative age of a timestamp ("3m ago"), or "—" if absent.
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

/// Short duration ("45s", "2m30s").
pub(crate) fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

/// Display label + colour for a liveness state.
pub(crate) fn liveness_label(l: Liveness) -> (&'static str, Color) {
    match l {
        Liveness::Busy => ("● busy", Color::Green),
        Liveness::Idle => ("○ idle", Color::Cyan),
        Liveness::Offline => ("× offline", Color::Red),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1_572_864), "1.5 MiB");
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
}
