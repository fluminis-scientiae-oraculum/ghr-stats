//! The fleet overview: host header + a runner table.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::{fmt_bytes, fmt_cpu, fmt_opt_bytes, fmt_uptime, liveness_label};
use crate::model::Liveness;
use crate::store::reader::ApiState;
use crate::tui::app::App;

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_table(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let (mut busy, mut idle, mut offline) = (0u32, 0u32, 0u32);
    for r in &app.runners {
        match r.liveness {
            Liveness::Busy => busy += 1,
            Liveness::Idle => idle += 1,
            Liveness::Offline => offline += 1,
        }
    }

    let counts = Line::from(vec![
        Span::styled(
            format!(" {} runners", app.runners.len()),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::raw("    "),
        Span::styled(format!("● {busy} busy"), Style::new().fg(Color::Green)),
        Span::raw("    "),
        Span::styled(format!("○ {idle} idle"), Style::new().fg(Color::Cyan)),
        Span::raw("    "),
        Span::styled(
            format!("× {offline} offline"),
            Style::new().fg(if offline > 0 {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
    ]);

    let host = match &app.host {
        Some(h) => {
            let mem_pct = if h.mem_total > 0 {
                h.mem_used as f64 / h.mem_total as f64 * 100.0
            } else {
                0.0
            };
            Line::from(format!(
                " load {:.2}    mem {}/{} ({:.0}%)    /tmp {}    free {}",
                h.load1,
                fmt_bytes(h.mem_used),
                fmt_bytes(h.mem_total),
                mem_pct,
                fmt_opt_bytes(h.tmp_bytes),
                fmt_opt_bytes(h.root_free),
            ))
        }
        None => Line::from(" sampling…"),
    };

    let github = if app.api_state.is_empty() {
        Line::from(Span::styled(
            " github: no API data (run `collect` with a PAT)",
            Style::new().fg(Color::DarkGray),
        ))
    } else {
        let online = app.api_state.values().filter(|s| s.online).count();
        let busy = app.api_state.values().filter(|s| s.busy).count();
        Line::from(format!(
            " github: {} known · {online} online · {busy} busy",
            app.api_state.len()
        ))
    };

    let para = Paragraph::new(vec![counts, host, github])
        .block(Block::bordered().title(" ghr-stats · fso-epoch "));
    f.render_widget(para, area);
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(["Runner", "Org", "Local", "GH", "CPU", "Mem", "Up"])
        .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));

    let rows = app.runners.iter().map(|r| {
        let (label, color) = liveness_label(r.liveness);
        Row::new(vec![
            Cell::from(r.name.clone()),
            Cell::from(r.org.clone()),
            Cell::from(Span::styled(label, Style::new().fg(color))),
            Cell::from(gh_span(r.gh)),
            Cell::from(fmt_cpu(r.cpu_pct)),
            Cell::from(fmt_opt_bytes(r.mem_bytes)),
            Cell::from(fmt_uptime(r.uptime_s)),
        ])
    });

    let widths = [
        Constraint::Length(20),
        Constraint::Length(26),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(6),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌")
        .block(Block::bordered().title(format!(" runners ({}) ", app.runners.len())));

    // Render needs &mut state; TableState is Copy, so copy to keep `app` shared.
    let mut state = app.table;
    f.render_stateful_widget(table, area, &mut state);
}

/// Compact GitHub-state glyph for the table's "GH" column.
fn gh_span(gh: Option<ApiState>) -> Span<'static> {
    match gh {
        Some(s) if s.busy => Span::styled("● busy", Style::new().fg(Color::Green)),
        Some(s) if s.online => Span::styled("○ idle", Style::new().fg(Color::Cyan)),
        Some(_) => Span::styled("× off", Style::new().fg(Color::Red)),
        None => Span::styled("–", Style::new().fg(Color::DarkGray)),
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let help = " ↑↓/jk move · Enter detail · Tab trends · w jobs · r refresh · q quit";
    let text = match &app.status {
        Some(s) => format!("{help}    [{s}]"),
        None => help.to_string(),
    };
    f.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}
