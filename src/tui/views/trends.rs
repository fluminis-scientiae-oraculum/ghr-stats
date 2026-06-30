//! Fleet-wide historical trends, read from SQLite: occupancy, host load,
//! memory, and disk (/tmp + aggregate `_work`) over time.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph, Sparkline};

use super::{fmt_bytes, fmt_opt_bytes};
use crate::tui::app::App;

const MIB: u64 = 1024 * 1024;

pub(crate) fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    if app.trend_host.is_empty() && app.trend_busy.is_empty() {
        f.render_widget(
            Paragraph::new("No history yet — start the collector:  ghr-stats collect")
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

    // Busy runners over time.
    let busy: Vec<u64> = app.trend_busy.iter().map(|b| b.busy as u64).collect();
    let (busy_now, online_now) = app
        .trend_busy
        .last()
        .map(|b| (b.busy, b.online))
        .unwrap_or((0, 0));
    spark(
        f,
        rows[0],
        format!(" busy runners   now {busy_now} / {online_now} online "),
        busy,
        Color::Yellow,
    );

    // Host load average.
    let load: Vec<u64> = app
        .trend_host
        .iter()
        .map(|h| (h.load1 * 100.0) as u64)
        .collect();
    let load_now = app.trend_host.last().map(|h| h.load1).unwrap_or(0.0);
    spark(
        f,
        rows[1],
        format!(" load1   now {load_now:.2} "),
        load,
        Color::Magenta,
    );

    // Memory used (percent).
    let mem: Vec<u64> = app
        .trend_host
        .iter()
        .map(|h| mem_pct(h.mem_used, h.mem_total))
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
    spark(f, rows[2], mem_title, mem, Color::Blue);

    // /tmp used.
    let tmp: Vec<u64> = app
        .trend_host
        .iter()
        .map(|h| h.tmp_bytes.unwrap_or(0) / MIB)
        .collect();
    let tmp_now = app.trend_host.last().and_then(|h| h.tmp_bytes);
    spark(
        f,
        rows[3],
        format!(" /tmp used   now {} ", fmt_opt_bytes(tmp_now)),
        tmp,
        Color::Red,
    );

    // Aggregate _work size (sampled on the slow cadence, so sparser).
    let work: Vec<u64> = app
        .trend_host
        .iter()
        .map(|h| h.work_bytes.unwrap_or(0) / MIB)
        .collect();
    let work_now = app.trend_host.iter().rev().find_map(|h| h.work_bytes);
    spark(
        f,
        rows[4],
        format!(" _work total   now {} ", fmt_opt_bytes(work_now)),
        work,
        Color::Green,
    );

    f.render_widget(
        Paragraph::new(" Esc/Tab back · r refresh · q quit")
            .style(Style::new().fg(Color::DarkGray)),
        outer[2],
    );
}

fn mem_pct(used: u64, total: u64) -> u64 {
    (used * 100).checked_div(total).unwrap_or(0)
}

fn spark(f: &mut Frame, area: Rect, title: String, data: Vec<u64>, color: Color) {
    let max = data.iter().copied().max().unwrap_or(0).max(1);
    f.render_widget(
        Sparkline::default()
            .block(Block::bordered().title(title))
            .data(data)
            .max(max)
            .style(Style::new().fg(color)),
        area,
    );
}
