//! Recent jobs across the fleet (from hook events; conclusion filled later by
//! the API reconcile).

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Cell, Padding, Paragraph, Row, Table, Wrap};

use super::{fmt_ago, fmt_dur};
use crate::hooks::install::HookStatus;
use crate::store::reader::JobRow;
use crate::tui::app::App;
use crate::tui::history::Mode;

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    if app.jobs.is_empty() {
        f.render_widget(
            Paragraph::new(empty_state(app)).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(" jobs ")
                    .padding(Padding::horizontal(1)),
            ),
            area,
        );
    } else {
        draw_table(f, app, area);
    }
}

/// The Jobs empty-state copy — hook-aware, so "no jobs yet" is never mistaken for
/// "hooks not installed". Ephemeral needs the collector; in Persistent mode the
/// message depends on whether the ghr-stats hook is actually installed on any
/// runner (if it is, we're simply waiting for a job to run).
fn empty_state(app: &App) -> String {
    if app.mode() == Mode::Ephemeral {
        return "Jobs are a Persistent-mode feature.\n\nInstall the collector to record job \
                starts and completions:  ghr-stats systemd install"
            .to_string();
    }
    let ours = app
        .runners
        .iter()
        .filter(|r| matches!(r.hook, HookStatus::Ours))
        .count();
    if ours > 0 {
        format!(
            "No jobs recorded yet.\n\nThe ghr-stats job hook is installed on {ours} runner(s) — \
             starts and completions will appear here as runners pick up work."
        )
    } else {
        "No jobs recorded yet.\n\nThe ghr-stats job hook isn't feeding any runner yet. Install \
         or chain it on the Config tab with [h] (as root), or run `sudo ghr-stats config`."
            .to_string()
    }
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
