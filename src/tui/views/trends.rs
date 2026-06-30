//! Fleet-wide historical trends, read from SQLite: occupancy, host load,
//! memory, and disk (/tmp + aggregate `_work`) over time. Each metric is a line
//! chart with a relative-time X axis and a 0-based Y axis (see
//! [`super::draw_time_chart`]); incomparable Y scales ⇒ one chart per metric.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph};

use super::{draw_time_chart, fmt_bytes, fmt_opt_bytes};
use crate::tui::app::App;

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    if app.trend_host.is_empty() && app.trend_busy.is_empty() {
        f.render_widget(
            Paragraph::new("No history yet — start the sampler:  ghr-stats serve")
                .block(Block::bordered().title(" fleet trends ")),
            area,
        );
        return;
    }

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let samples = app.trend_host.len().max(app.trend_busy.len());
    f.render_widget(
        Paragraph::new(format!(" fleet trends · last {samples} samples"))
            .style(Style::new().fg(Color::DarkGray)),
        outer[0],
    );

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Ratio(1, 5); 5])
        .split(outer[1]);

    // Busy runners over time (Y capped at peak online ⇒ axis reflects capacity).
    let busy_pts: Vec<(f64, f64)> = app
        .trend_busy
        .iter()
        .map(|b| (b.ts as f64, b.busy as f64))
        .collect();
    let (busy_now, online_now) = app
        .trend_busy
        .last()
        .map(|b| (b.busy, b.online))
        .unwrap_or((0, 0));
    let busy_max = app
        .trend_busy
        .iter()
        .map(|b| b.online)
        .max()
        .unwrap_or(0)
        .max(1);
    draw_time_chart(
        f,
        rows[0],
        &format!(" busy runners   now {busy_now} / {online_now} online "),
        &busy_pts,
        [0.0, busy_max as f64],
        vec!["0".to_string(), busy_max.to_string()],
        Color::Yellow,
    );

    // Host load average.
    let load_pts: Vec<(f64, f64)> = app
        .trend_host
        .iter()
        .map(|h| (h.ts as f64, h.load1))
        .collect();
    let load_now = app.trend_host.last().map(|h| h.load1).unwrap_or(0.0);
    let load_max = app
        .trend_host
        .iter()
        .map(|h| h.load1)
        .fold(0.0_f64, f64::max)
        .max(0.1);
    draw_time_chart(
        f,
        rows[1],
        &format!(" load1   now {load_now:.2} "),
        &load_pts,
        [0.0, load_max],
        vec!["0".to_string(), format!("{load_max:.1}")],
        Color::Magenta,
    );

    // Memory used (percent — natural 0..100 scale).
    let mem_pts: Vec<(f64, f64)> = app
        .trend_host
        .iter()
        .map(|h| (h.ts as f64, mem_pct(h.mem_used, h.mem_total) as f64))
        .collect();
    let mem_title = match app.trend_host.last() {
        Some(h) if h.mem_total > 0 => format!(
            " mem   now {}% ({} / {}) ",
            mem_pct(h.mem_used, h.mem_total),
            fmt_bytes(h.mem_used),
            fmt_bytes(h.mem_total)
        ),
        _ => " mem ".to_string(),
    };
    draw_time_chart(
        f,
        rows[2],
        &mem_title,
        &mem_pts,
        [0.0, 100.0],
        vec!["0".to_string(), "100%".to_string()],
        Color::Blue,
    );

    // /tmp used (raw bytes; skip ticks that didn't sample it).
    let tmp_pts: Vec<(f64, f64)> = app
        .trend_host
        .iter()
        .filter_map(|h| h.tmp_bytes.map(|b| (h.ts as f64, b as f64)))
        .collect();
    let tmp_now = app.trend_host.last().and_then(|h| h.tmp_bytes);
    let tmp_max = app
        .trend_host
        .iter()
        .filter_map(|h| h.tmp_bytes)
        .max()
        .unwrap_or(0)
        .max(1);
    draw_time_chart(
        f,
        rows[3],
        &format!(" /tmp used   now {} ", fmt_opt_bytes(tmp_now)),
        &tmp_pts,
        [0.0, tmp_max as f64],
        vec!["0".to_string(), fmt_bytes(tmp_max)],
        Color::Red,
    );

    // Aggregate _work size (sampled on the slow cadence, so sparser).
    let work_pts: Vec<(f64, f64)> = app
        .trend_host
        .iter()
        .filter_map(|h| h.work_bytes.map(|b| (h.ts as f64, b as f64)))
        .collect();
    let work_now = app.trend_host.iter().rev().find_map(|h| h.work_bytes);
    let work_max = app
        .trend_host
        .iter()
        .filter_map(|h| h.work_bytes)
        .max()
        .unwrap_or(0)
        .max(1);
    draw_time_chart(
        f,
        rows[4],
        &format!(" _work total   now {} ", fmt_opt_bytes(work_now)),
        &work_pts,
        [0.0, work_max as f64],
        vec!["0".to_string(), fmt_bytes(work_max)],
        Color::Green,
    );

    f.render_widget(
        Paragraph::new(" Tab/1-4 switch · r refresh · q quit")
            .style(Style::new().fg(Color::DarkGray)),
        outer[2],
    );
}

fn mem_pct(used: u64, total: u64) -> u64 {
    (used * 100).checked_div(total).unwrap_or(0)
}
