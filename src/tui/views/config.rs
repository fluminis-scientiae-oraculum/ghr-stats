//! The Config tab: resolved paths, mode + collector status, configured tokens,
//! and metrics settings, plus the in-TUI actions — `[a]` add org+PAT (native
//! wizard), `[h]` install hooks, `[m]` toggle metrics, `[o]` open the file. The
//! CLI `ghr-stats config` remains for a full guided first-run.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::hooks::install::HookStatus;
use crate::paths::Scope;
use crate::tui::app::{App, LiveRunner};
use crate::tui::history::Mode;

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

    lines.push(heading("Mode"));
    let mode = app.mode();
    let (label, color) = match mode {
        Mode::Persistent => ("Persistent", Color::Green),
        Mode::Ephemeral => ("Ephemeral", Color::Yellow),
    };
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
                    format!("  history is served by the {} collector", scope_word(scope)),
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
                "  ghr-stats systemd install",
                Style::new().fg(Color::Cyan),
            )));
            // A unit exists on disk but no socket answered ⇒ nudge to start it.
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
    // Config: show the file actually present on this host (probing both scopes),
    // with a REDACTED count read from that same file — not the loaded cfg's — so a
    // system config isn't reported "not written" from a non-root dashboard.
    let cfg_path = installed_config(&app.config_target());
    let cfg_state = match std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|t| crate::config::count_tokens(&t))
    {
        Some(n) => format!("{}  ({n} token(s))", cfg_path.display()),
        None if cfg_path.exists() => format!("{}  (present, unreadable)", cfg_path.display()),
        None => format!("{}  (not written)", cfg_path.display()),
    };
    lines.push(kv("config", &cfg_state));
    // Service + binary: report the scope whose artifact is actually on disk, not
    // the one the current euid would use. This dashboard is normally run non-root
    // while a system install lives under /etc + /usr/local/bin — keying off
    // `Scope::detect()` made a present install read as "not installed".
    let svc = match installed_scope(Scope::systemd_unit_path) {
        Some(s) => format!("installed ({} scope)", scope_word(s)),
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

/// The scope whose artifact `path_of(scope)` actually exists on disk (System
/// first — the privileged, canonical deployment), or `None` if neither does.
/// Lets install status be reported independent of the euid the read-only TUI
/// runs under; the pure selection is [`pick_installed`].
fn installed_scope(path_of: impl Fn(Scope) -> PathBuf) -> Option<Scope> {
    pick_installed(|s| path_of(s).exists())
}

/// The first scope (System, then User) for which `exists` holds. Pure — split
/// from the disk probe so it is unit-tested.
fn pick_installed(exists: impl Fn(Scope) -> bool) -> Option<Scope> {
    [Scope::System, Scope::User]
        .into_iter()
        .find(|s| exists(*s))
}

/// The config file present on this host: the resolved read/write `target` first
/// (an explicit `--config`, the sudo-invoker's home, or the current scope), then
/// each scope's canonical file. Falls back to `target` (where it WOULD be
/// written) when none exist. Keeps the host-inventory line honest across scopes.
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
        // A system install viewed from a non-root TUI must resolve to System,
        // not "not installed" — the cross-scope status bug this guards.
        assert_eq!(pick_installed(|s| s == Scope::System), Some(Scope::System));
        assert_eq!(pick_installed(|s| s == Scope::User), Some(Scope::User));
        assert_eq!(pick_installed(|_| false), None);
        // Both present ⇒ System wins (the privileged, canonical deployment).
        assert_eq!(pick_installed(|_| true), Some(Scope::System));
    }
}
