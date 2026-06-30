//! Recent jobs across the fleet (from hook events; conclusion filled later by
//! the API reconcile).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::{fmt_ago, fmt_dur};
use crate::store::reader::JobRow;
use crate::tui::app::App;

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    if app.jobs.is_empty() {
        f.render_widget(
            Paragraph::new(
                "No job events yet.\n\nInstall the runner hooks (see packaging/hooks/) and point \
                 the runners' ACTIONS_RUNNER_HOOK_JOB_STARTED / _COMPLETED at them.",
            )
            .block(Block::bordered().title(" jobs ")),
            chunks[0],
        );
    } else {
        draw_table(f, app, chunks[0]);
    }

    f.render_widget(
        Paragraph::new(" Esc/w back · r refresh · q quit").style(Style::new().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Runner", "Repo · Job", "Started", "Duration", "Result"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));

    let rows = app.jobs.iter().map(|j| {
        Row::new(vec![
            Cell::from(j.runner_name.clone()),
            Cell::from(format!("{} · {}", j.repo, j.job)),
            Cell::from(fmt_ago(j.started_at)),
            Cell::from(duration(j)),
            Cell::from(result_span(j)),
        ])
    });

    let widths = [
        Constraint::Length(20),
        Constraint::Min(24),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(format!(" recent jobs ({}) ", app.jobs.len())));
    f.render_widget(table, area);
}

fn duration(j: &JobRow) -> String {
    match (j.started_at, j.completed_at) {
        (Some(s), Some(c)) if c >= s => fmt_dur((c - s) as u64),
        (Some(_), None) => "running".to_string(),
        _ => "—".to_string(),
    }
}

/// Result cell: API conclusion if known, else a coarse state from the timing.
fn result_span(j: &JobRow) -> Span<'static> {
    match j.conclusion.as_deref() {
        Some("success") => Span::styled("success", Style::new().fg(Color::Green)),
        Some("failure") => Span::styled("failure", Style::new().fg(Color::Red)),
        Some(other) => Span::styled(other.to_string(), Style::new().fg(Color::Yellow)),
        None if j.completed_at.is_some() => Span::styled("done", Style::new().fg(Color::Gray)),
        None if j.started_at.is_some() => Span::styled("running", Style::new().fg(Color::Cyan)),
        None => Span::raw("—"),
    }
}
