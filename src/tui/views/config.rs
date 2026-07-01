//! The Config tab: resolved paths, sampler status, configured tokens, and
//! metrics settings, plus the in-TUI actions — `[a]` add org+PAT (native
//! wizard), `[h]` install hooks, `[m]` toggle metrics, `[o]` open the file. The
//! CLI `ghr-stats config` remains for a full guided first-run.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::hooks::install::HookStatus;
use crate::paths::Scope;
use crate::tui::app::{App, LiveRunner};

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
    lines.push(Line::from(Span::styled(
        "  pull: scrape /metrics into Prometheus/Grafana · push: POST JSON to OpenObserve",
        Style::new().fg(Color::DarkGray),
    )));
    lines.push(Line::raw(""));

    // Install & teardown (#uninstall): what's on this host + how to remove it.
    // Read-only — the removal itself is the CLI verb `ghr-stats uninstall`.
    lines.push(heading("Install & teardown"));
    let scope = Scope::detect();
    let cfg_path = app.config_target();
    let cfg_state = if cfg_path.exists() {
        // Redacted: a COUNT, never a token value.
        let n = cfg.github.tokens.len() + usize::from(cfg.github.token.is_some());
        format!("{}  ({n} token(s))", cfg_path.display())
    } else {
        format!("{}  (not written)", cfg_path.display())
    };
    lines.push(kv("config", &cfg_state));
    let unit = scope.systemd_unit_path();
    let svc = if unit.exists() {
        format!("installed ({} scope)", scope_word(scope))
    } else {
        "not installed".to_string()
    };
    lines.push(kv("service", &svc));
    let bin = scope.bin_path();
    let bin_state = if bin.exists() {
        format!("{} (installed)", bin.display())
    } else {
        format!("{} (not installed)", bin.display())
    };
    lines.push(kv("binary", &bin_state));
    lines.push(kv("hooks", &hooks_summary(&app.runners)));
    lines.push(Line::from(Span::styled(
        "  Teardown: `ghr-stats uninstall` (dry-run plan) · `uninstall all` removes everything",
        Style::new().fg(Color::DarkGray),
    )));

    // First-run invite (#4): if nothing is discoverable/configured, point at the
    // actions rather than leaving a dead end. Otherwise the footer's [a]/[h]/[m]/
    // [o] hints suffice.
    if cfg.runner_roots.is_empty() || cfg.github.tokens.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  First run? [a] add an org + PAT · [h] install hooks · or `ghr-stats config`.",
            Style::new().fg(Color::Yellow),
        )));
    }

    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" config ")
                .padding(Padding::horizontal(1)),
        ),
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

fn hooks_summary(runners: &[LiveRunner]) -> String {
    summarize_hooks(runners.iter().map(|r| r.hook))
}

/// Bucket hook statuses into a one-line dashboard summary. Pure (takes statuses,
/// not the heavy `LiveRunner`) so it is unit-tested.
fn summarize_hooks(statuses: impl Iterator<Item = HookStatus>) -> String {
    let (mut ours, mut foreign, mut unset, mut unreadable, mut total) = (0, 0, 0, 0, 0);
    for s in statuses {
        total += 1;
        match s {
            HookStatus::Ours => ours += 1,
            HookStatus::Foreign => foreign += 1,
            HookStatus::Unset => unset += 1,
            HookStatus::Unreadable => unreadable += 1,
        }
    }
    if total == 0 {
        return "(no runners discovered)".to_string();
    }
    format!(
        "{ours} ours · {foreign} foreign · {unset} unset · {unreadable} unreadable  (of {total})"
    )
}

fn scope_word(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "system",
        Scope::User => "user",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_hooks_buckets_and_counts() {
        let s = summarize_hooks(
            [
                HookStatus::Ours,
                HookStatus::Foreign,
                HookStatus::Ours,
                HookStatus::Unset,
            ]
            .into_iter(),
        );
        assert_eq!(s, "2 ours · 1 foreign · 1 unset · 0 unreadable  (of 4)");
        assert_eq!(
            summarize_hooks(std::iter::empty()),
            "(no runners discovered)"
        );
    }
}
