//! Per-runner detail: identity + live stats + CPU/mem history sparklines.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use super::{
    ChartSpec, draw_time_chart, fmt_bytes, fmt_cpu, fmt_dur, fmt_opt_bytes, fmt_uptime,
    liveness_label,
};
use crate::store::reader::ApiState;
use crate::tui::app::App;
use crate::util::now_epoch;

/// GitHub's view of a runner for the detail panel.
fn gh_text(gh: Option<ApiState>) -> String {
    match gh {
        Some(s) if s.busy => "online, busy".to_string(),
        Some(s) if s.online => "online, idle".to_string(),
        Some(_) => "offline".to_string(),
        None => "(no API data — needs a PAT + collector)".to_string(),
    }
}

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(r) = app.detail_runner() else {
        f.render_widget(
            Paragraph::new("no runner selected").block(Block::bordered().title(" runner ")),
            area,
        );
        return;
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(0)])
        .split(area);

    let (label, color) = liveness_label(r.liveness);
    let since = r
        .state_seconds
        .map(|s| format!("  for {}", fmt_dur(s.max(0) as u64)))
        .unwrap_or_default();
    let info = vec![
        Line::from(vec![
            Span::styled(r.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("   {}", r.org)),
        ]),
        Line::from(vec![
            Span::raw("state   "),
            Span::styled(label, Style::new().fg(color)),
            Span::raw(since),
            Span::raw(format!("    hook {}", r.hook.glyph())),
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
        Line::from(format!("job     {}", active_job_text(app))),
    ];
    f.render_widget(
        Paragraph::new(info).block(
            Block::bordered()
                .title(" runner detail ")
                .padding(Padding::horizontal(1)),
        ),
        chunks[0],
    );

    draw_charts(f, app, chunks[1]);
}

/// The runner's in-flight job (from local hook events), or "—".
fn active_job_text(app: &App) -> String {
    match &app.detail_active_job {
        Some(j) => {
            let elapsed = j
                .started_at
                .map(|s| fmt_dur((now_epoch() - s).max(0) as u64))
                .unwrap_or_else(|| "?".to_string());
            format!("{} · {}  (running {elapsed})", j.repo, j.job)
        }
        None => "—".to_string(),
    }
}

fn draw_charts(f: &mut Frame, app: &App, area: Rect) {
    if app.detail_history.is_empty() {
        f.render_widget(
            Paragraph::new("No history yet — start the sampler:  ghr-stats serve")
                .block(Block::bordered().title(" history ")),
            area,
        );
        return;
    }

    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // One clock read per frame for both charts' relative-time X labels.
    let now = now_epoch();

    // CPU% over time (skip ticks with no cgroup reading; Y peak data-driven —
    // a busy job can exceed 100% across cores).
    let cpu_pts: Vec<(f64, f64)> = app
        .detail_history
        .iter()
        .filter_map(|p| p.cpu_pct.map(|c| (p.ts as f64, c as f64)))
        .collect();
    let cpu_now = app.detail_history.last().and_then(|p| p.cpu_pct);
    let cpu_max = app
        .detail_history
        .iter()
        .filter_map(|p| p.cpu_pct)
        .fold(0.0_f32, f32::max)
        .max(1.0);
    draw_time_chart(
        f,
        halves[0],
        now,
        ChartSpec {
            title: &format!(" cpu   now {}   peak {cpu_max:.1}% ", fmt_cpu(cpu_now)),
            points: &cpu_pts,
            y_bounds: [0.0, cpu_max as f64],
            y_labels: vec!["0".to_string(), format!("{cpu_max:.0}%")],
            color: Color::Cyan,
        },
    );

    // Memory (raw bytes; labels in binary units).
    let mem_pts: Vec<(f64, f64)> = app
        .detail_history
        .iter()
        .filter_map(|p| p.mem_bytes.map(|m| (p.ts as f64, m as f64)))
        .collect();
    let mem_now = app.detail_history.last().and_then(|p| p.mem_bytes);
    let mem_max = app
        .detail_history
        .iter()
        .filter_map(|p| p.mem_bytes)
        .max()
        .unwrap_or(0)
        .max(1);
    draw_time_chart(
        f,
        halves[1],
        now,
        ChartSpec {
            title: &format!(
                " mem   now {}   peak {} ",
                fmt_opt_bytes(mem_now),
                fmt_bytes(mem_max)
            ),
            points: &mem_pts,
            y_bounds: [0.0, mem_max as f64],
            y_labels: vec!["0".to_string(), fmt_bytes(mem_max)],
            color: Color::Green,
        },
    );
}
