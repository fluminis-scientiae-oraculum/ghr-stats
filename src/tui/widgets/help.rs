//! The `[?]` help sheet and the informational block — read-only centered popups
//! (see [`crate::tui::app::Overlay`]), dismissed by any key. Kept separate from
//! the wizard: help/info carry no state and never write anything.

use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::tui::view::centered_rect;
use crate::tui::viewmodel;

/// The full keymap + action reference + how to run as root. Not per-context: a
/// single sheet you can open anywhere to see everything the TUI does (the
/// footer is the per-context quick-ref; this is the manual).
pub(crate) fn draw_help(f: &mut Frame) {
    let mut lines = vec![
        section("Navigation"),
        key("Tab · 1–4", "switch tab"),
        key("↑↓ · j k", "move selection (Summary)"),
        key("Enter", "open runner detail"),
        key("Esc", "back · close a popup"),
        key("r", "refresh now"),
        key("?", "this help"),
        key("q", "quit"),
        blank(),
        section("Runner detail actions"),
        key(
            "R",
            "restart the runner service — reclaims the agent's GC RAM",
        ),
        key(
            "C",
            "recycle (idle only) — purge this runner's own _work/_temp + _diag",
        ),
        blank(),
        section("Config actions"),
        key("a", "manage org PATs — add / replace / remove (native wizard)"),
        key("h", "install / repair the runner job hooks (needs root)"),
        key("m", "toggle the Prometheus /metrics endpoint"),
        key("o", "open the config file in $EDITOR"),
        blank(),
        section("Modes"),
        Line::from(Span::styled(
            "   EPHEMERAL   live dashboard only — in-memory, since launch",
            Style::new().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "   PERSISTENT  + history · jobs · GitHub · metrics (install the collector):",
            Style::new().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            format!("               {}", viewmodel::copy::INSTALL_COLLECTOR),
            Style::new().fg(Color::Cyan),
        )),
        blank(),
        section("Running as root"),
    ];
    for l in crate::shared::privileged::root_guidance().lines() {
        lines.push(Line::from(format!("  {l}")).style(Style::new().fg(Color::Gray)));
    }
    lines.push(blank());
    lines.push(dismiss_hint());

    let area = centered_rect(74, 84, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_style(Style::new().fg(Color::Cyan))
                .title(" help "),
        ),
        area,
    );
}

/// A read-only info block (title + wrapped body). Used for the privilege
/// guidance — informational, never an error.
pub(crate) fn draw_info(f: &mut Frame, title: &str, body: &str) {
    let mut lines = vec![blank()];
    for l in body.lines() {
        lines.push(Line::from(format!("  {l}")).style(Style::new().fg(Color::Gray)));
    }
    lines.push(blank());
    lines.push(dismiss_hint());

    let area = centered_rect(70, 60, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_style(Style::new().fg(Color::Yellow))
                .title(format!(" {title} ")),
        ),
        area,
    );
}

fn section(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {s}"),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn key(k: &str, desc: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("   {k:<11}"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc.to_string(), Style::new().fg(Color::Gray)),
    ])
}

fn dismiss_hint() -> Line<'static> {
    Line::from(Span::styled(
        "  press any key to close",
        Style::new().fg(Color::DarkGray),
    ))
}

fn blank() -> Line<'static> {
    Line::from("")
}
