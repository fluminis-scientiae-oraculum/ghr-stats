//! Rendering. A top tab bar + one module per view; shared formatting helpers
//! live here. Views render into a given `Rect` (the area below the tab bar).

mod config;
mod jobs;
mod overview;
mod runner;
mod trends;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Clear, Dataset, GraphType, Paragraph, Wrap};

use crate::model::Liveness;
use crate::tui::action::ConfirmPrompt;
use crate::tui::app::{App, Hits, Tab};
use crate::util::now_epoch;

/// The one keymap footer, shown verbatim in every view (bracket every key). A
/// single consistent surface — the same keys wherever you are; the per-view
/// action keys ([Enter]/[R]/[C]) only fire where they apply.
const FOOTER: &str = "[↑↓/jk] move · [Enter] detail · [Tab] switch · [R] restart · \
                      [C] recycle · [r] refresh · [q] quit";

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    // Small-terminal guard: below this the table/charts smear — say so instead.
    if area.width < 40 || area.height < 8 {
        f.render_widget(
            Paragraph::new("terminal too small\n(min 40×8)")
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }

    // A shared bottom footer row, so every view gets the same keymap and views
    // no longer each hand-roll one. Body = everything between the bar and it.
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    draw_tab_bar(f, app, rows[0]);
    let body = rows[1];

    if app.drill.is_some() {
        runner::draw(f, app, body);
    } else {
        match app.tab {
            Tab::Summary => overview::draw(f, app, body),
            Tab::Jobs => jobs::draw(f, app, body),
            Tab::Trends => trends::draw(f, app, body),
            Tab::Config => config::draw(f, app, body),
            Tab::Quit => {}
        }
    }
    draw_footer(f, app, rows[2]);
}

/// The shared footer: the keymap left-aligned, plus the last action's status
/// right-aligned (highlighted) when there is one. The keymap always wins the
/// left edge, so it stays readable even when a status is present.
fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let keymap = Paragraph::new(FOOTER).style(Style::new().fg(Color::DarkGray));
    match app.status.as_deref() {
        Some(s) => {
            let sw = (s.chars().count() as u16).saturating_add(3).min(area.width);
            let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(sw)]).split(area);
            f.render_widget(keymap, cols[0]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(" {s} "),
                    Style::new().fg(Color::Black).bg(Color::Cyan),
                ))
                .alignment(Alignment::Right),
                cols[1],
            );
        }
        None => f.render_widget(keymap, area),
    }
}

/// The clickable top tab bar. Records each tab's x-range into `app.hits` so the
/// mouse handler can resolve clicks (ratatui is immediate-mode).
fn draw_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    let mut tabs = Vec::new();
    let mut x = area.x;
    for (i, t) in Tab::BAR.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", Style::new().fg(Color::DarkGray)));
            x += 3;
        }
        let label = format!(" {} ", t.label());
        let w = label.chars().count() as u16;
        let style = if *t == Tab::Quit {
            Style::new().fg(Color::Red)
        } else if *t == app.tab {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        tabs.push((*t, x, x + w));
        spans.push(Span::styled(label, style));
        x += w;
    }
    // Reset the hit cache each frame; the Summary view re-populates `table_rows`
    // when it draws (this runs first, so it must not clobber a later write).
    *app.hits.borrow_mut() = Hits {
        tabs,
        tab_row: area.y,
        table_rows: None,
    };
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A centered confirm popup for a pending action. Typestate-driven: there is no
/// overlay variant for it, so it cannot be rendered without a pending action.
pub(crate) fn draw_confirm(f: &mut Frame, prompt: &ConfirmPrompt) {
    let area = centered_rect(60, 30, f.area());
    f.render_widget(Clear, area);
    let border = if prompt.danger {
        Color::Red
    } else {
        Color::Yellow
    };
    let lines = vec![
        Line::from(""),
        Line::from(prompt.body.clone()),
        Line::from(""),
        Line::from(Span::styled(
            " [y] confirm    [n] cancel ",
            Style::new().add_modifier(Modifier::REVERSED),
        )),
    ];
    let popup = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .border_style(Style::new().fg(border))
            .title(format!(" {} ", prompt.title)),
    );
    f.render_widget(popup, area);
}

/// A rectangle `pct_x`% × `pct_y`% of `area`, centered. Shared by the confirm
/// popup and the config wizard overlay.
pub(crate) fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let vy = (100 - pct_y) / 2;
    let vx = (100 - pct_x) / 2;
    let col = Layout::vertical([
        Constraint::Percentage(vy),
        Constraint::Percentage(pct_y),
        Constraint::Percentage(vy),
    ])
    .split(area)[1];
    Layout::horizontal([
        Constraint::Percentage(vx),
        Constraint::Percentage(pct_x),
        Constraint::Percentage(vx),
    ])
    .split(col)[1]
}

/// Middle-ellipsize a string to at most `max` display chars ("fso-e…r-00").
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

/// One metric as a line chart with a relative-time X axis (oldest … now) and a
/// 0-based Y axis — the readable replacement for an axis-less `Sparkline`.
///
/// `points` are `(ts_secs, value)` oldest → newest; X bounds/labels come from
/// those timestamps (so gaps plot at true wall-clock positions), while the
/// caller supplies the Y bounds + labels because value formatting is
/// metric-specific (count vs percent vs bytes). At most three labels per axis —
/// ratatui mis-positions a fourth (issue 334). Fewer than two points ⇒ a
/// "collecting" note, since a line needs two ends.
pub(crate) fn draw_time_chart(
    f: &mut Frame,
    area: Rect,
    title: &str,
    points: &[(f64, f64)],
    y_bounds: [f64; 2],
    y_labels: Vec<String>,
    color: Color,
) {
    if points.len() < 2 {
        f.render_widget(
            Paragraph::new("  collecting…")
                .style(Style::new().fg(Color::DarkGray))
                .block(Block::bordered().title(title.to_string())),
            area,
        );
        return;
    }
    let now = now_epoch();
    let t0 = points[0].0;
    let tn = points[points.len() - 1].0;
    let x_labels = vec![
        rel_label(t0 as i64, now),
        rel_label(((t0 + tn) / 2.0) as i64, now),
        "now".to_string(),
    ];
    let datasets = vec![
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(color))
            .data(points),
    ];
    let axis_style = Style::new().fg(Color::DarkGray);
    let chart = Chart::new(datasets)
        .block(Block::bordered().title(title.to_string()))
        .x_axis(
            Axis::default()
                .style(axis_style)
                .bounds([t0, tn])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(axis_style)
                .bounds(y_bounds)
                .labels(y_labels),
        );
    f.render_widget(chart, area);
}

/// A timestamp's age relative to `now`, as a short axis label: seconds under a
/// minute, whole minutes under an hour, else `h`+`m`. "now" at the right edge.
/// Distinct from [`fmt_dur`] (m+s precision), since an axis label wants round
/// granularity at the scale it spans, not down-to-the-second noise on a 5h window.
fn rel_label(ts: i64, now: i64) -> String {
    let age = (now - ts).max(0) as u64;
    match age {
        0 => "now".to_string(),
        s if s < 60 => format!("-{s}s"),
        s if s < 3_600 => format!("-{}m", s / 60),
        s => format!("-{}h{}m", s / 3_600, (s % 3_600) / 60),
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

    #[test]
    fn relative_axis_labels() {
        let now = 10_000;
        assert_eq!(rel_label(now, now), "now");
        assert_eq!(rel_label(now - 45, now), "-45s");
        assert_eq!(rel_label(now - 90, now), "-1m");
        assert_eq!(rel_label(now - (5 * 3600 + 9 * 60), now), "-5h9m");
        // future timestamps (clock skew) clamp to "now", never a positive age
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

    /// Golden-frame snapshot of the confirm popup, rendered into ratatui's
    /// in-memory `TestBackend` — the CI-able answer to "is the layout right?",
    /// replacing eyeballed tmux captures. Deterministic (no wall-clock).
    /// Run `cargo insta review` to accept intended changes.
    #[test]
    fn snapshot_confirm_popup() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let prompt = ConfirmPrompt {
            title: "Recycle runner-01 (#1)".to_string(),
            body: "stop · purge _work/_temp · trim _diag · start\n(scoped to THIS runner \
                   only — never global /tmp or docker; idle-only)"
                .to_string(),
            danger: true,
        };
        term.draw(|f| draw_confirm(f, &prompt)).unwrap();
        insta::assert_snapshot!(term.backend());
    }
}
