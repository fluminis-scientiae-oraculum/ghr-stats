//! Rendering. A top tab bar + one module per view; the chrome that frames them
//! lives here. Views render into a given `Rect` (the area below the tab bar).
//!
//! This file draws; [`fmt`] does not. Everything left here takes a `Frame` — the
//! tab bar, the footer, the modal confirm, the shared time chart — while every
//! function in [`fmt`] turns a value into a display token and can be tested
//! without a terminal. That is why the two snapshot tests stayed here and the
//! five formatting tests went with their subjects.
//!
//! The footer is the reason [`footer_items`] returns a `KeyCode` per hint rather
//! than acting: a click on a hint and the key it depicts must take the SAME path,
//! so the two can never drift.
//!
//! [`fmt`]'s items are re-exported flat, so each view keeps saying
//! `use super::{fmt_ago, fmt_dur}` unchanged.

mod config;
mod fmt;
mod jobs;
mod overview;
mod runner;
mod trends;

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart, Clear, Dataset, GraphType, Paragraph, Wrap};

use crate::tui::app::{App, Hits, Tab};
use crate::tui::input::action::ConfirmPrompt;
use crate::tui::viewmodel;

use fmt::rel_label;
pub(crate) use fmt::{
    ellipsize_middle, fmt_ago, fmt_bytes, fmt_cpu, fmt_dur, fmt_opt_bytes, fmt_uptime,
    liveness_label,
};

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

/// Context-aware key hints in bracket format — only the keys that act where you
/// are, so the footer changes with the view/mode instead of advertising
/// Detail-only actions on the list tabs. Each item pairs its display text with
/// the key it triggers (`None` for a non-actionable hint like navigation), so
/// the footer is rendered AND click-dispatched from one source.
fn footer_items(app: &App) -> Vec<(Option<KeyCode>, &'static str)> {
    if app.drill.is_some() {
        // Runner detail: the per-runner actions live here, not on the lists.
        return vec![
            (Some(KeyCode::Esc), "[Esc] back"),
            (Some(KeyCode::Char('R')), "[R] restart"),
            (Some(KeyCode::Char('C')), "[C] recycle"),
            (Some(KeyCode::Char('r')), "[r] refresh"),
            (Some(KeyCode::Char('?')), "[?] help"),
            (Some(KeyCode::Char('q')), "[q] quit"),
        ];
    }
    match app.tab {
        Tab::Summary => vec![
            (None, "[↑↓/jk] move"),
            (Some(KeyCode::Enter), "[Enter] detail"),
            (Some(KeyCode::Tab), "[Tab] switch"),
            (Some(KeyCode::Char('r')), "[r] refresh"),
            (Some(KeyCode::Char('?')), "[?] help"),
            (Some(KeyCode::Char('q')), "[q] quit"),
        ],
        Tab::Config => vec![
            (Some(KeyCode::Char('a')), "[a] org"),
            (Some(KeyCode::Char('h')), "[h] hooks"),
            (Some(KeyCode::Char('m')), "[m] metrics"),
            (Some(KeyCode::Char('o')), "[o] open"),
            (Some(KeyCode::Tab), "[Tab] switch"),
            (Some(KeyCode::Char('?')), "[?] help"),
            (Some(KeyCode::Char('q')), "[q] quit"),
        ],
        _ => vec![
            (Some(KeyCode::Tab), "[Tab] switch"),
            (Some(KeyCode::Char('r')), "[r] refresh"),
            (Some(KeyCode::Char('?')), "[?] help"),
            (Some(KeyCode::Char('q')), "[q] quit"),
        ],
    }
}

/// The shared footer: the keymap left-aligned, plus the last action's status
/// right-aligned (highlighted) when there is one. The keymap always wins the
/// left edge, so it stays readable even when a status is present.
fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // Build the footer from its items, recording each actionable hint's column
    // range into the hit cache so `on_mouse` can turn a click into that keystroke.
    const SEP: &str = " · ";
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = Vec::new();
    let mut clicks: Vec<(KeyCode, u16, u16)> = Vec::new();
    let mut x = area.x;
    for (i, (key, label)) in footer_items(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(SEP, dim));
            x += SEP.chars().count() as u16;
        }
        let w = label.chars().count() as u16;
        if let Some(k) = key {
            clicks.push((k, x, x + w));
        }
        spans.push(Span::styled(label, dim));
        x += w;
    }
    {
        let mut hits = app.hits.borrow_mut();
        hits.footer = clicks;
        hits.footer_row = area.y;
    }
    let keymap = Paragraph::new(Line::from(spans));
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
        ..Default::default()
    };
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    // Mode badge, right-aligned on the tab-bar row — only when it won't collide
    // with the tabs (`x` is the column just past the last tab).
    let (label, color) = viewmodel::style::mode_badge(app.mode());
    let badge = format!(" {label} ");
    if (x - area.x) as usize + badge.chars().count() < area.width as usize {
        f.render_widget(
            Paragraph::new(Span::styled(
                badge,
                Style::new()
                    .fg(Color::Black)
                    .bg(color)
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
            area,
        );
    }
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

/// One metric as a line chart with a relative-time X axis (oldest … now) and a
/// 0-based Y axis — the readable replacement for an axis-less `Sparkline`.
///
/// `points` are `(ts_secs, value)` oldest → newest; X bounds/labels come from
/// those timestamps (so gaps plot at true wall-clock positions), while the
/// caller supplies the Y bounds + labels because value formatting is
/// metric-specific (count vs percent vs bytes). At most three labels per axis —
/// ratatui mis-positions a fourth (ratatui issue 334). Fewer than two points ⇒ a
/// "collecting" note, since a line needs two ends.
///
/// `now` (the reference for the relative-time X labels) is a parameter, not read
/// from the clock here — so the callers pass one `now_epoch()` per frame and
/// tests can pin it for deterministic golden snapshots.
///
/// The chart's *content* (title, series, Y axis, color) is bundled in
/// [`ChartSpec`] so the call stays `(where, when, what)` rather than a long
/// positional argument list.
pub(crate) struct ChartSpec<'a> {
    pub title: &'a str,
    pub points: &'a [(f64, f64)],
    pub y_bounds: [f64; 2],
    pub y_labels: Vec<String>,
    pub color: Color,
    /// An optional second series drawn over the first, for comparing two views
    /// of the same quantity. Its points are supplied independently, so ticks
    /// missing from it simply are not plotted — a GAP, not a zero, which is the
    /// difference between "we did not ask" and "the answer was none".
    pub overlay: Option<(&'a [(f64, f64)], Color)>,
}

pub(crate) fn draw_time_chart(f: &mut Frame, area: Rect, now: i64, spec: ChartSpec) {
    if spec.points.len() < 2 {
        f.render_widget(
            Paragraph::new("  collecting…")
                .style(Style::new().fg(Color::DarkGray))
                .block(Block::bordered().title(spec.title.to_string())),
            area,
        );
        return;
    }
    let t0 = spec.points[0].0;
    let tn = spec.points[spec.points.len() - 1].0;
    let x_labels = vec![
        rel_label(t0 as i64, now),
        rel_label(((t0 + tn) / 2.0) as i64, now),
        "now".to_string(),
    ];
    let mut datasets = vec![
        Dataset::default()
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().fg(spec.color))
            .data(spec.points),
    ];
    if let Some((pts, color)) = spec.overlay
        && pts.len() >= 2
    {
        datasets.push(
            Dataset::default()
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(color))
                .data(pts),
        );
    }
    let axis_style = Style::new().fg(Color::DarkGray);
    let chart = Chart::new(datasets)
        .block(Block::bordered().title(spec.title.to_string()))
        .x_axis(
            Axis::default()
                .style(axis_style)
                .bounds([t0, tn])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .style(axis_style)
                .bounds(spec.y_bounds)
                .labels(spec.y_labels),
        );
    f.render_widget(chart, area);
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Golden a time-series chart. Deterministic because `now` and the points are
    /// fixed — the whole reason `draw_time_chart` takes `now` as a parameter
    /// instead of reading the clock. Pins the axes, the relative-time X labels
    /// (`-2m · -1m · now`), and the braille line.
    #[test]
    fn snapshot_time_chart() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let now = 10_000_i64;
        let points = vec![
            ((now - 120) as f64, 10.0),
            ((now - 60) as f64, 25.0),
            (now as f64, 40.0),
        ];
        term.draw(|f| {
            let area = f.area();
            draw_time_chart(
                f,
                area,
                now,
                ChartSpec {
                    title: " cpu   now 40% ",
                    points: &points,
                    y_bounds: [0.0, 40.0],
                    y_labels: vec!["0".to_string(), "40%".to_string()],
                    color: Color::Cyan,
                    overlay: None,
                },
            );
        })
        .unwrap();
        insta::assert_snapshot!(term.backend());
    }
}
