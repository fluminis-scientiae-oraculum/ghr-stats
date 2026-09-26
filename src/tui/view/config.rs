use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::shared::hooks::install::HookStatus;
use crate::shared::models::Mode;
use crate::shared::paths::Scope;
use crate::tui::app::{App, LiveRunner};
use crate::tui::viewmodel;

pub(crate) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let cfg = app.cfg();
    let mut lines: Vec<Line> = Vec::new();

    lines.push(heading("Paths"));
    lines.push(kv("database", &cfg.db_path.display().to_string()));
    lines.push(kv(
        "event log",
        &format!(
            "<runner-dir>/{} (per runner)",
            crate::shared::hooks::RUNNER_EVENT_LOG
        ),
    ));
    let roots = if cfg.runner_roots.is_empty() {
        format!(
            "(none — set with `{}`)",
            crate::shared::privileged::sudo_hint("config")
        )
    } else {
        cfg.runner_roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    lines.push(kv("runner roots", &roots));
    lines.push(Line::raw(""));

    lines.push(heading("Mode"));
    let mode = app.mode();
    let (label, color) = viewmodel::style::mode_word(mode);
    lines.push(Line::from(vec![
        key("mode"),
        Span::styled(label, Style::new().fg(color).add_modifier(Modifier::BOLD)),
    ]));
    match mode {
        Mode::Persistent => {
            let scope = app.source_scope().unwrap_or_else(Scope::detect);
            lines.push(kv(
                "collector",
                &format!("connected · {}", scope.socket_path().display()),
            ));
            if scope != Scope::detect() {
                lines.push(Line::from(Span::styled(
                    format!("  history is served by the {} collector", scope.label()),
                    Style::new().fg(Color::DarkGray),
                )));
            }
        }
        Mode::Ephemeral => {
            lines.push(Line::from(Span::styled(
                "  live-only — install the collector for history, jobs, GitHub + metrics:",
                Style::new().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {}", viewmodel::copy::INSTALL_COLLECTOR),
                Style::new().fg(Color::Cyan),
            )));
            if installed_scope(Scope::systemd_unit_path).is_some() {
                lines.push(Line::from(Span::styled(
                    "  (service installed but not reachable — `systemctl [--user] start ghr-stats`)",
                    Style::new().fg(Color::Yellow),
                )));
            }
        }
    }
    lines.push(Line::raw(""));

    lines.push(heading("GitHub tokens (read-only PATs)"));
    // Not `cfg.github.tokens`: a non-root TUI cannot read the root-owned config.
    let orgs = app.configured_orgs();
    if orgs.is_empty() {
        // In Persistent mode the collector's empty answer is authoritative.
        let unreadable = mode == Mode::Ephemeral && {
            let p = installed_config(&app.config_target());
            p.exists() && std::fs::read_to_string(&p).is_err()
        };
        lines.push(Line::from(Span::styled(
            if unreadable {
                format!(
                    "  (configured, but the root-owned config isn't readable here — run `{}`)",
                    crate::shared::privileged::sudo_hint("")
                )
            } else {
                "  (none configured)".to_string()
            },
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for org in orgs {
            // Not `key`: org logins can exceed its 16 columns.
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

    lines.push(heading("Version"));
    lines.push(kv("binary", crate::shared::util::BUILD_VERSION));
    let vstate = viewmodel::status::version_state(
        crate::shared::util::BUILD_VERSION,
        app.collector_version(),
        app.mode(),
    );
    let collector = match (app.collector_version(), vstate) {
        (Some(v), _) => v.to_string(),
        (None, viewmodel::status::VersionState::CollectorUnknown) => {
            "connected, version not reported (older build)".to_string()
        }
        (None, _) => "—  (no collector)".to_string(),
    };
    lines.push(kv("collector", &collector));
    lines.push(kv("ipc wire", &format!("v{}", crate::shared::ipc::VERSION)));
    if let Some(warning) = viewmodel::copy::version_warning(vstate, app.ephemeral_reason()) {
        lines.push(Line::from(Span::styled(
            format!("  {warning}"),
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::raw(""));

    lines.push(heading("Install & teardown"));
    let cfg_path = installed_config(&app.config_target());
    let cfg_state = match std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|t| crate::shared::config::count_tokens(&t))
    {
        Some(n) => format!("{}  ({n} token(s))", cfg_path.display()),
        None if cfg_path.exists() => format!("{}  (present, unreadable)", cfg_path.display()),
        None => format!("{}  (not written)", cfg_path.display()),
    };
    lines.push(kv("config", &cfg_state));
    let svc = match installed_scope(Scope::systemd_unit_path) {
        Some(s) => format!("installed ({} scope)", s.label()),
        None => "not installed".to_string(),
    };
    lines.push(kv("service", &svc));
    let bin_state = match installed_scope(Scope::bin_path) {
        Some(s) => format!("{} (installed)", s.bin_path().display()),
        None => format!("{} (not installed)", Scope::detect().bin_path().display()),
    };
    lines.push(kv("binary", &bin_state));
    lines.push(kv("hooks", &hooks_summary(&app.runners)));
    lines.push(Line::from(Span::styled(
        "  Teardown: `ghr-stats uninstall` (dry-run plan) · `uninstall all` removes everything",
        Style::new().fg(Color::DarkGray),
    )));

    if cfg.runner_roots.is_empty() || app.configured_orgs().is_empty() {
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

/// Not `Scope::detect()`: the TUI usually runs non-root against a System install.
fn installed_scope(path_of: impl Fn(Scope) -> PathBuf) -> Option<Scope> {
    pick_installed(|s| path_of(s).exists())
}

fn pick_installed(exists: impl Fn(Scope) -> bool) -> Option<Scope> {
    [Scope::System, Scope::User]
        .into_iter()
        .find(|s| exists(*s))
}

fn installed_config(target: &Path) -> PathBuf {
    [
        target.to_path_buf(),
        Scope::System.config_file(),
        Scope::User.config_file(),
    ]
    .into_iter()
    .find(|p| p.exists())
    .unwrap_or_else(|| target.to_path_buf())
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

    #[test]
    fn pick_installed_prefers_system_then_user() {
        assert_eq!(pick_installed(|s| s == Scope::System), Some(Scope::System));
        assert_eq!(pick_installed(|s| s == Scope::User), Some(Scope::User));
        assert_eq!(pick_installed(|_| false), None);
        assert_eq!(pick_installed(|_| true), Some(Scope::System));
    }
}
