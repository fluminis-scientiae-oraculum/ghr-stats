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
use crate::shared::models::{ApiState, JobRow};
use crate::shared::util::now_epoch;
use crate::tui::app::App;
use crate::tui::viewmodel;

/// GitHub's view of a runner for the detail panel. When there's no data, the
/// viewmodel decides the actual cause (mode / missing PAT / reconcile / not-seen).
fn gh_text(gh: Option<ApiState>, app: &App) -> String {
    match gh {
        Some(s) if s.busy => "online, busy".to_string(),
        Some(s) if s.online => "online, idle".to_string(),
        Some(_) => "offline".to_string(),
        None => viewmodel::copy::runner_github_cell(viewmodel::status::runner_github_absent(
            app.mode(),
            app.has_tokens(),
            app.reconcile_populated(),
        )),
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
        Line::from(format!("github  {}", gh_text(r.gh, app))),
        Line::from(format!("job     {}", last_job_text(app))),
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

/// The runner's most recent job line, resolving the clock once.
fn last_job_text(app: &App) -> String {
    render_last_job(app.detail_last_job.as_ref(), now_epoch())
}

/// Pure renderer for the detail "job" line: "running Xs" while in-flight, else
/// "<conclusion>, Xs ago" for the last completed one, or "—" if the runner has
/// never run a job. Split out from [`last_job_text`] so it is testable without an
/// `App` or the wall clock.
fn render_last_job(job: Option<&JobRow>, now: i64) -> String {
    let Some(j) = job else {
        return "—".to_string();
    };
    let label = format!("{} · {}", j.repo, j.job);
    match (j.started_at, j.completed_at) {
        // In-flight: no completion yet.
        (Some(s), None) => format!("{label}  (running {})", fmt_dur((now - s).max(0) as u64)),
        // Completed: the API conclusion (or a neutral "done") + how long ago.
        (_, Some(c)) => {
            let outcome = j.conclusion.as_deref().unwrap_or("done");
            format!(
                "{label}  ({outcome}, {} ago)",
                fmt_dur((now - c).max(0) as u64)
            )
        }
        (None, None) => label,
    }
}

fn draw_charts(f: &mut Frame, app: &App, area: Rect) {
    if app.detail_history.is_empty() {
        f.render_widget(
            Paragraph::new(viewmodel::copy::collecting_sparkline())
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

#[cfg(test)]
mod tests {
    use super::render_last_job;
    use crate::shared::models::JobRow;

    fn job(started: Option<i64>, completed: Option<i64>, conclusion: Option<&str>) -> JobRow {
        JobRow {
            runner_name: "runner-01".into(),
            repo: "example-org/foo".into(),
            job: "build".into(),
            started_at: started,
            completed_at: completed,
            conclusion: conclusion.map(str::to_string),
        }
    }

    #[test]
    fn no_job_renders_dash() {
        assert_eq!(render_last_job(None, 1_000), "—");
    }

    #[test]
    fn in_flight_job_shows_running_elapsed() {
        let j = job(Some(1_000), None, None);
        // now 90s after start.
        assert_eq!(
            render_last_job(Some(&j), 1_090),
            "example-org/foo · build  (running 1m30s)"
        );
    }

    #[test]
    fn completed_job_shows_conclusion_and_age() {
        let j = job(Some(1_000), Some(1_100), Some("success"));
        // now 30s after completion.
        assert_eq!(
            render_last_job(Some(&j), 1_130),
            "example-org/foo · build  (success, 30s ago)"
        );
        // Conclusion not yet reconciled ⇒ neutral "done".
        let j = job(Some(1_000), Some(1_100), None);
        assert_eq!(
            render_last_job(Some(&j), 1_100),
            "example-org/foo · build  (done, 0s ago)"
        );
    }
}
