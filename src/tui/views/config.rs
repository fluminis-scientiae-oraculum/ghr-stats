//! The Config tab: resolved paths, sampler status, configured tokens, and
//! metrics settings. Press `[a]` to add an org + read-only PAT via the native
//! in-TUI wizard (`tui::wizard`); the full flow (hooks, metrics) stays on the
//! CLI `ghr-stats config`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::tui::app::App;

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let cfg = app.cfg();
    let mut lines: Vec<Line> = Vec::new();

    lines.push(heading("Paths"));
    lines.push(kv("database", &cfg.db_path.display().to_string()));
    lines.push(kv("event log", &cfg.event_log.display().to_string()));
    let roots = if cfg.runner_roots.is_empty() {
        "(none — set with `ghr-stats config`)".to_string()
    } else {
        cfg.runner_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(kv("runner roots", &roots));
    lines.push(Line::raw(""));

    lines.push(heading("Sampler"));
    let (status, color) = if app.serve_up {
        ("running", Color::Green)
    } else {
        (
            "stopped — run `ghr-stats serve` for history + metrics",
            Color::Yellow,
        )
    };
    lines.push(Line::from(vec![
        key("status"),
        Span::styled(status, Style::new().fg(color)),
    ]));
    lines.push(Line::raw(""));

    lines.push(heading("GitHub tokens (read-only PATs)"));
    if cfg.github.tokens.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none configured)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for org in cfg.github.tokens.keys() {
            // Natural spacing (not the fixed 16-col `key`): org logins can exceed
            // 16 chars and would otherwise run into "present".
            lines.push(Line::from(vec![
                Span::styled(format!("  {org}  "), Style::new().fg(Color::Gray)),
                Span::styled("present", Style::new().fg(Color::Green)),
            ]));
        }
    }
    lines.push(Line::raw(""));

    lines.push(heading("Metrics"));
    let pull = if cfg.metrics.pull.enabled {
        format!("on · {}", cfg.metrics.pull.addr)
    } else {
        "off".to_string()
    };
    lines.push(kv("pull (/metrics)", &pull));
    let push = if cfg.metrics.push.enabled {
        "on".to_string()
    } else {
        "off".to_string()
    };
    lines.push(kv("push", &push));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  [a] add org + read-only PAT   ·   `ghr-stats config` for hooks + metrics",
        Style::new().fg(Color::DarkGray),
    )));

    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" config ")),
        area,
    );
}

fn heading(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn key(k: &str) -> Span<'static> {
    Span::styled(format!("  {k:<16}"), Style::new().fg(Color::Gray))
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![key(k), Span::raw(v.to_string())])
}
