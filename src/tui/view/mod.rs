//! Rendering: one module per view, plus the shared chrome (tab bar, footer, confirm, chart).

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
    if area.width < 40 || area.height < 8 {
        f.render_widget(
            Paragraph::new("terminal too small\n(min 40×8)")
                .alignment(Alignment::Center)
                .style(Style::new().fg(Color::Yellow)),
            area,
        );
        return;
    }

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

/// Only keys that act in the current view; `None` marks a hint that is not clickable.
fn footer_items(app: &App) -> Vec<(Option<KeyCode>, &'static str)> {
    if app.drill.is_some() {
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

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
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
    // Runs before the views draw, so Summary's `table_rows` write lands after this reset.
    *app.hits.borrow_mut() = Hits {
        tabs,
        tab_row: area.y,
        table_rows: None,
        ..Default::default()
    };
    f.render_widget(Paragraph::new(Line::from(spans)), area);

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

pub(crate) struct ChartSpec<'a> {
    pub title: &'a str,
    /// `(ts_secs, value)`, oldest → newest.
    pub points: &'a [(f64, f64)],
    pub y_bounds: [f64; 2],
    /// At most three: ratatui mis-positions a fourth (ratatui issue 334).
    pub y_labels: Vec<String>,
    pub color: Color,
    /// Ticks missing here plot as a gap, not zero.
    pub overlay: Option<(&'a [(f64, f64)], Color)>,
}

/// `now` is a parameter, not the clock, so snapshots are deterministic.
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
