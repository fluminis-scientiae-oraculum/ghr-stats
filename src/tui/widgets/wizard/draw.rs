//! What the operator sees: the wizard popup, drawn over the dashboard.
//!
//! Separated from the machine because it is reached by a different caller —
//! `tui::mod` draws, `tui::app::mutate` keys — and because every ratatui drawing
//! type in this module lives here and nowhere else. The rendering has also
//! changed alone twice (`263f490`, `888d653`), which is the seam test's third
//! leg: past commits landed on ONE side of it.
//!
//! The masking is a SECURITY property, not a cosmetic one: [`input_line`] renders
//! the PAT buffer as `•` so the secret cannot reach the screen, and therefore
//! cannot reach a snapshot, a tmux capture, or a screen-share. It is 1:1 per
//! char, so the cursor still tracks the real caret.
//!
//! The tests below fabricate states directly rather than driving the transitions
//! — legitimate only because a child module can reach its ancestor's private
//! fields, and the right trade here: a snapshot test should pin one state's
//! appearance without depending on the path taken to reach it.

use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use tui_input::Input;

use super::WizardMode;

/// Render the centered wizard popup over the dashboard.
pub(crate) fn draw(f: &mut Frame, mode: &WizardMode) {
    let area = crate::tui::view::centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);

    let (title, lines) = match mode {
        WizardMode::PickAction(_) => (
            " Configure ",
            vec![
                Line::from(""),
                Line::from("  [a]  add / replace org + read-only PAT"),
                Line::from("  [r]  remove org (drops its PAT)"),
                Line::from(""),
                footer("[a] add/replace · [r] remove · [Esc] close"),
            ],
        ),
        WizardMode::OrgInput(w) => (
            " Add org ",
            vec![
                Line::from(""),
                input_line("GitHub org login", &w.state.org, false),
                Line::from(""),
                footer("[Enter] next · [Esc] cancel"),
            ],
        ),
        WizardMode::PatInput(w) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::raw("  org  "),
                    Span::styled(
                        w.state.org.clone(),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                ]),
                input_line("Fine-grained PAT (github_pat_…)", &w.state.pat, true),
                Line::from(Span::styled(
                    "  needs Self-hosted runners: Read  (+ Actions: Read for job results)",
                    Style::new().fg(Color::DarkGray),
                )),
            ];
            if let Some(err) = &w.state.error {
                lines.push(Line::from(Span::styled(
                    format!("  ✗ {err}"),
                    Style::new().fg(Color::Red),
                )));
            }
            lines.push(Line::from(""));
            lines.push(footer("[Enter] validate · [Esc] cancel"));
            (" Add PAT ", lines)
        }
        WizardMode::Confirmed(w) => (
            " Confirm ",
            vec![
                Line::from(""),
                Line::from(format!(
                    "  {} — valid, {}/{} local runners matched.",
                    w.state.org, w.state.matched, w.state.local
                )),
                Line::from("  Save this read-only token to the config (0600)?"),
                Line::from(""),
                footer("[y] save · [n] cancel"),
            ],
        ),
        WizardMode::RemoveOrgInput(w) => (
            " Remove org ",
            vec![
                Line::from(""),
                input_line("GitHub org login to remove", &w.state.org, false),
                Line::from(""),
                footer("[Enter] next · [Esc] cancel"),
            ],
        ),
        WizardMode::RemoveConfirm(w) => (
            " Confirm remove ",
            vec![
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Remove the read-only PAT for "),
                    Span::styled(
                        w.state.org.clone(),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" and forget the org?"),
                ]),
                Line::from(""),
                footer("[y] remove · [n] cancel"),
            ],
        ),
        WizardMode::Done(w) => {
            let color = if w.state.ok { Color::Green } else { Color::Red };
            let glyph = if w.state.ok { "✓" } else { "✗" };
            (
                " Done ",
                vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  {glyph} {}", w.state.message),
                        Style::new().fg(color),
                    )),
                    Line::from(""),
                    footer("[any key] close"),
                ],
            )
        }
    };

    let popup = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::bordered()
            .border_style(Style::new().fg(Color::Cyan))
            .title(title),
    );
    f.render_widget(popup, area);
}

/// A labelled input field showing the value with a reverse-video cursor at the
/// widget's cursor position. `masked` renders the value as `•` (the PAT); the
/// cursor still tracks the real caret since the mask is 1:1 per char.
fn input_line(label: &str, input: &Input, masked: bool) -> Line<'static> {
    let value = input.value();
    let shown: Vec<char> = if masked {
        std::iter::repeat_n('•', value.chars().count()).collect()
    } else {
        value.chars().collect()
    };
    let cursor = input.visual_cursor().min(shown.len());
    let before: String = shown[..cursor].iter().collect();
    let (at, after): (String, String) = if cursor < shown.len() {
        (
            shown[cursor].to_string(),
            shown[cursor + 1..].iter().collect(),
        )
    } else {
        (" ".to_string(), String::new())
    };
    let bold = Style::new().add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::new().fg(Color::Gray)),
        Span::styled(before, bold),
        Span::styled(at, Style::new().add_modifier(Modifier::REVERSED)),
        Span::styled(after, bold),
    ])
}

fn footer(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {s}"),
        Style::new().fg(Color::DarkGray),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{
        Confirmed, Done, OrgInput, PatInput, RemoveConfirm, RemoveOrgInput, Wizard,
    };
    use super::*;

    /// Render a wizard state into an in-memory `TestBackend` and flatten it to
    /// text — the deterministic, CI-able answer to "does it draw right?".
    fn render(mode: &WizardMode) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, mode)).unwrap();
        format!("{}", term.backend())
    }

    /// SECURITY: the PAT buffer must render masked — the secret must never reach
    /// the screen (nor, therefore, a snapshot / tmux capture / screen-share).
    #[test]
    fn masked_pat_never_renders_the_secret() {
        let mode = WizardMode::PatInput(Wizard {
            state: PatInput {
                org: "example-org".to_string(),
                pat: Input::from("github_pat_SUPERSECRETVALUE".to_string()),
                error: None,
            },
        });
        let out = render(&mode);
        assert!(
            !out.contains("SUPERSECRET"),
            "PAT leaked into render:\n{out}"
        );
        assert!(out.contains('•'), "masked bullets not rendered:\n{out}");
        assert!(out.contains("example-org"), "org context missing:\n{out}");
    }

    #[test]
    fn snapshot_pick_action() {
        insta::assert_snapshot!(render(&WizardMode::new()));
    }

    #[test]
    fn snapshot_org_input() {
        let mode = WizardMode::OrgInput(Wizard {
            state: OrgInput {
                org: Input::from("example-org".to_string()),
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_pat_input_with_rejection() {
        let mode = WizardMode::PatInput(Wizard {
            state: PatInput {
                org: "example-org".to_string(),
                pat: Input::from("github_pat_abcd".to_string()),
                error: Some("token lacks 'Self-hosted runners: Read' on example-org".to_string()),
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_confirmed() {
        let mode = WizardMode::Confirmed(Wizard {
            state: Confirmed {
                org: "example-org".to_string(),
                pat: "github_pat_abcd".to_string(),
                matched: 3,
                local: 4,
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_done_ok() {
        let mode = WizardMode::Done(Wizard {
            state: Done {
                message: "saved read-only token for example-org (3/4 local runners matched)"
                    .to_string(),
                ok: true,
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_remove_org_input() {
        let mode = WizardMode::RemoveOrgInput(Wizard {
            state: RemoveOrgInput {
                org: Input::from("example-org".to_string()),
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_remove_confirm() {
        let mode = WizardMode::RemoveConfirm(Wizard {
            state: RemoveConfirm {
                org: "example-org".to_string(),
            },
        });
        insta::assert_snapshot!(render(&mode));
    }
}
