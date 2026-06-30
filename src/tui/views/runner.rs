//! Per-runner detail: identity + live stats + CPU/mem history sparklines.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Sparkline};

use super::{fmt_cpu, fmt_opt_bytes, fmt_uptime, liveness_label};
use crate::store::reader::ApiState;
use crate::tui::app::App;

const MIB: u64 = 1024 * 1024;

/// GitHub's view of a runner for the detail panel.
fn gh_text(gh: Option<ApiState>) -> String {
    match gh {
        Some(s) if s.busy => "online, busy".to_string(),
        Some(s) if s.online => "online, idle".to_string(),
        Some(_) => "offline".to_string(),
        None => "(no API data — needs a PAT + collector)".to_string(),
    }
}

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let Some(r) = app.selected_runner() else {
        f.render_widget(
            Paragraph::new("no runner selected").block(Block::bordered().title(" runner ")),
            area,
        );
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let (label, color) = liveness_label(r.liveness);
    let info = vec![
        Line::from(vec![
            Span::styled(r.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("   {}", r.org)),
        ]),
        Line::from(vec![
            Span::raw("state   "),
            Span::styled(label, Style::new().fg(color)),
        ]),
        Line::from(format!(
            "group   {}",
            r.group.clone().unwrap_or_else(|| "—".to_string())
        )),
        Line::from(format!("agent   #{}    user {}", r.agent_id, r.user)),
        Line::from(format!("dir     {}", r.dir.display())),
        Line::from(format!(
            "live    cpu {}   mem {}   up {}",
            fmt_cpu(r.cpu_pct),
            fmt_opt_bytes(r.mem_bytes),
            fmt_uptime(r.uptime_s)
        )),
        Line::from(format!("github  {}", gh_text(r.gh))),
    ];
    f.render_widget(
        Paragraph::new(info).block(Block::bordered().title(" runner detail ")),
        chunks[0],
    );

    draw_charts(f, app, chunks[1]);

    f.render_widget(
        Paragraph::new(" Esc back · r refresh · q quit").style(Style::new().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn draw_charts(f: &mut Frame, app: &App, area: Rect) {
    if app.detail_history.is_empty() {
        f.render_widget(
            Paragraph::new("No history yet — start the collector:  ghr-stats collect")
                .block(Block::bordered().title(" history ")),
            area,
        );
        return;
    }

    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // CPU%, kept at 0.1% resolution as an integer bar height.
    let cpu: Vec<u64> = app
        .detail_history
        .iter()
        .map(|p| (p.cpu_pct.unwrap_or(0.0).max(0.0) * 10.0) as u64)
        .collect();
    let cpu_max = cpu.iter().copied().max().unwrap_or(0).max(1);
    let cpu_now = app.detail_history.last().and_then(|p| p.cpu_pct);
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(format!(
                " cpu   now {}   peak {:.1}% ",
                fmt_cpu(cpu_now),
                cpu_max as f64 / 10.0
            )))
            .data(cpu)
            .max(cpu_max)
            .style(Style::new().fg(Color::Cyan)),
        halves[0],
    );

    // Memory in MiB.
    let mem: Vec<u64> = app
        .detail_history
        .iter()
        .map(|p| p.mem_bytes.unwrap_or(0) / MIB)
        .collect();
    let mem_max = mem.iter().copied().max().unwrap_or(0).max(1);
    let mem_now = app.detail_history.last().and_then(|p| p.mem_bytes);
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(format!(
                " mem   now {}   peak {} MiB ",
                fmt_opt_bytes(mem_now),
                mem_max
            )))
            .data(mem)
            .max(mem_max)
            .style(Style::new().fg(Color::Green)),
        halves[1],
    );
}
