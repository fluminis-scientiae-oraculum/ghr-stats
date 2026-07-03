//! The fleet Summary: a host header (with a GitHub-view line in Persistent mode,
//! or an "install the collector" hint in Ephemeral) and a responsive,
//! ellipsized runner table.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};

use super::{
    ellipsize_middle, fmt_bytes, fmt_cpu, fmt_dur, fmt_opt_bytes, fmt_uptime, liveness_label,
};
use crate::shared::hooks::install::HookStatus;
use crate::shared::models::{ApiState, Liveness};
use crate::tui::app::App;
use crate::tui::viewmodel;

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    // The shared keymap footer is drawn by the parent; this view owns only its
    // header + table.
    let chunks = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).split(area);
    draw_header(f, app, chunks[0]);
    draw_table(f, app, chunks[1]);
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
        None => Line::from(" host: no data"),
    };

    // Third line: the GitHub fleet summary when data is present, else the reason
    // it's absent — derived once in the viewmodel, never re-decided here.
    let third = match viewmodel::status::github_reason(
        app.mode(),
        app.has_tokens(),
        app.reconcile_populated(),
    ) {
        None => {
            let online = app.api_state.values().filter(|s| s.online).count();
            let gbusy = app.api_state.values().filter(|s| s.busy).count();
            Line::from(format!(
                " github: {} known · {online} online · {gbusy} busy",
                app.api_state.len()
            ))
        }
        Some(reason) => Line::from(Span::styled(
            format!(" github: {}", viewmodel::copy::github_summary_hint(reason)),
            Style::new().fg(Color::DarkGray),
        )),
    };

    let para =
        Paragraph::new(vec![counts, host, third]).block(Block::bordered().title(" ghr-stats "));
    f.render_widget(para, area);
}

fn draw_table(f: &mut Frame, app: &App, area: Rect) {
    // Responsive: the metric columns are fixed; the Runner/Org text columns
    // share the remaining width and middle-ellipsize to fit. `For` = time in the
    // current liveness (#16); `Hook` = job-hook status (#27).
    let inner_w = area.width.saturating_sub(2) as usize;
    let fixed = 9 + 7 + 6 + 8 + 8 + 10 + 6 + 8; // Local,For,Hook,GH,CPU,Mem,Up + spacing
    let flex = inner_w.saturating_sub(fixed).max(18);
    let name_w = (flex * 42 / 100).clamp(9, 28);
    let org_w = flex.saturating_sub(name_w).clamp(8, 30);

    let header = Row::new([
        "Runner", "Org", "Local", "For", "Hook", "GH", "CPU", "Mem", "Up",
    ])
    .style(Style::new().fg(Color::Gray).add_modifier(Modifier::BOLD));

    let rows = app.runners.iter().map(|r| {
        let (label, color) = liveness_label(r.liveness);
        Row::new(vec![
            Cell::from(ellipsize_middle(&r.name, name_w)),
            Cell::from(ellipsize_middle(&r.org, org_w)),
            Cell::from(Span::styled(label, Style::new().fg(color))),
            Cell::from(state_for(r.state_seconds)),
            Cell::from(hook_span(r.hook)),
            Cell::from(gh_span(r.gh)),
            Cell::from(fmt_cpu(r.cpu_pct)),
            Cell::from(fmt_opt_bytes(r.mem_bytes)),
            Cell::from(fmt_uptime(r.uptime_s)),
        ])
    });

    let widths = [
        Constraint::Length(name_w as u16),
        Constraint::Length(org_w as u16),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(6),
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

    // Render needs `&mut TableState`; borrow the interior-mutable state so
    // ratatui's auto-scroll offset is written BACK to `app.table` (not discarded
    // into a throwaway copy) — that's what keeps click-to-select accurate once the
    // list scrolls past one screen.
    let mut state = app.table.borrow_mut();
    f.render_stateful_widget(table, area, &mut state);

    // Cache the data-row region (inside the border, below the header) so a click
    // there selects the runner under the cursor.
    app.hits.borrow_mut().table_rows = Some(Rect {
        x: area.x + 1,
        y: area.y + 2,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(3),
    });
}

/// Time held in the current liveness state ("2d14h", "5m"), or "—".
fn state_for(secs: Option<i64>) -> String {
    secs.map(|s| fmt_dur(s.max(0) as u64))
        .unwrap_or_else(|| "—".to_string())
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

/// Job-hook status glyph, colored by severity: ours (green ✓), a foreign hook
/// (yellow ✗ — chain/instruct), none (red ✗), unreadable (gray ?).
fn hook_span(h: HookStatus) -> Span<'static> {
    let color = match h {
        HookStatus::Ours => Color::Green,
        HookStatus::Foreign => Color::Yellow,
        HookStatus::Unset => Color::Red,
        HookStatus::Unreadable => Color::DarkGray,
    };
    Span::styled(h.glyph(), Style::new().fg(color))
}
