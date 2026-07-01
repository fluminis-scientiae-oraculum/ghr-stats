//! Native in-TUI config wizard — a typestate popup. No CLI escape, no terminal
//! teardown: configuration happens *in* the dashboard.
//!
//! The compile-time contract (the whole reason this is safe): `write` exists
//! ONLY on `Wizard<Confirmed>`, and a `Confirmed` is reachable ONLY from a
//! successful `Wizard<PatInput>::validate`. So a rejected or un-validated PAT
//! can never be persisted — it does not compile. The PAT buffer is rendered
//! masked (`•`) and never logged.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::config::persist;
use crate::github::validate::{self, Verdict};

/// What the wizard needs from the app to act: the locally-discovered runner
/// agentIds (for the agentId-confirm) and the config file to write.
pub(crate) struct WizardCtx {
    pub local_ids: HashSet<i64>,
    pub target: PathBuf,
}

// ---- typestate states (data-carrying; private fields ⇒ un-fabricable) ----

pub(crate) struct PickAction;
pub(crate) struct OrgInput {
    org: String,
}
pub(crate) struct PatInput {
    org: String,
    pat: String,
    error: Option<String>,
}
pub(crate) struct Confirmed {
    org: String,
    pat: String,
    matched: usize,
    local: usize,
}
pub(crate) struct Done {
    message: String,
    ok: bool,
}

pub(crate) struct Wizard<S> {
    state: S,
}

impl Wizard<PickAction> {
    fn add_org(self) -> Wizard<OrgInput> {
        Wizard {
            state: OrgInput { org: String::new() },
        }
    }
}

impl Wizard<OrgInput> {
    fn push(&mut self, c: char) {
        // Org logins are ASCII alnum + hyphen; ignore anything else.
        if c.is_ascii_alphanumeric() || c == '-' {
            self.state.org.push(c);
        }
    }
    fn backspace(&mut self) {
        self.state.org.pop();
    }
    /// Advance to PAT entry — only with a non-empty org (else stay put).
    fn next(self) -> Result<Wizard<PatInput>, Wizard<OrgInput>> {
        if self.state.org.trim().is_empty() {
            return Err(self);
        }
        Ok(Wizard {
            state: PatInput {
                org: self.state.org.trim().to_string(),
                pat: String::new(),
                error: None,
            },
        })
    }
}

impl Wizard<PatInput> {
    fn push(&mut self, c: char) {
        if !c.is_control() {
            self.state.pat.push(c);
        }
    }
    fn backspace(&mut self) {
        self.state.pat.pop();
    }
    /// Validate the PAT (sync `github::validate`). Success ⇒ `Confirmed`;
    /// rejection ⇒ back to `PatInput` for THAT org, prefilled, PAT cleared, with
    /// the reason shown (feedback #6).
    fn validate(self, local_ids: &HashSet<i64>) -> Result<Wizard<Confirmed>, Wizard<PatInput>> {
        match validate::validate(&self.state.pat, &self.state.org, local_ids) {
            Verdict::Valid { matched, local, .. } => Ok(Wizard {
                state: Confirmed {
                    org: self.state.org,
                    pat: self.state.pat,
                    matched,
                    local,
                },
            }),
            Verdict::Rejected(why) => Err(Wizard {
                state: PatInput {
                    org: self.state.org,
                    pat: String::new(),
                    error: Some(why),
                },
            }),
        }
    }
}

impl Wizard<Confirmed> {
    /// Persist the validated token. The ONLY persist path — reachable only from a
    /// successful `validate`.
    fn write(self, target: &Path) -> Wizard<Done> {
        let done = match persist::set_org_token(target, &self.state.org, &self.state.pat) {
            Ok(()) => Done {
                message: format!(
                    "saved read-only token for {} ({}/{} local runners matched)",
                    self.state.org, self.state.matched, self.state.local
                ),
                ok: true,
            },
            Err(e) => Done {
                message: format!("write failed: {e}"),
                ok: false,
            },
        };
        Wizard { state: done }
    }
}

/// The loop-owned runtime enum (the typestate changes type each transition, but
/// the loop needs one fixed type). Per-state methods stay compile-time-guarded.
pub(crate) enum WizardMode {
    PickAction(Wizard<PickAction>),
    OrgInput(Wizard<OrgInput>),
    PatInput(Wizard<PatInput>),
    Confirmed(Wizard<Confirmed>),
    Done(Wizard<Done>),
}

/// What the loop should do after a key.
pub(crate) enum Step {
    Stay(WizardMode),
    /// Close the popup; `true` if the config changed (⇒ reload).
    Close(bool),
}

impl WizardMode {
    pub(crate) fn new() -> Self {
        WizardMode::PickAction(Wizard { state: PickAction })
    }

    /// Route one key press. Consumes `self` (the typestate) and returns the next
    /// mode or a close. `validate`/`write` block briefly (a sync network call /
    /// a file write) — acceptable for a one-shot config step.
    pub(crate) fn on_key(self, code: KeyCode, ctx: &WizardCtx) -> Step {
        match self {
            WizardMode::PickAction(w) => match code {
                KeyCode::Char('a') => Step::Stay(WizardMode::OrgInput(w.add_org())),
                KeyCode::Esc => Step::Close(false),
                _ => Step::Stay(WizardMode::PickAction(w)),
            },
            WizardMode::OrgInput(mut w) => match code {
                KeyCode::Esc => Step::Close(false),
                KeyCode::Backspace => {
                    w.backspace();
                    Step::Stay(WizardMode::OrgInput(w))
                }
                KeyCode::Enter => match w.next() {
                    Ok(next) => Step::Stay(WizardMode::PatInput(next)),
                    Err(same) => Step::Stay(WizardMode::OrgInput(same)),
                },
                KeyCode::Char(c) => {
                    w.push(c);
                    Step::Stay(WizardMode::OrgInput(w))
                }
                _ => Step::Stay(WizardMode::OrgInput(w)),
            },
            WizardMode::PatInput(mut w) => match code {
                KeyCode::Esc => Step::Close(false),
                KeyCode::Backspace => {
                    w.backspace();
                    Step::Stay(WizardMode::PatInput(w))
                }
                KeyCode::Enter => match w.validate(&ctx.local_ids) {
                    Ok(confirmed) => Step::Stay(WizardMode::Confirmed(confirmed)),
                    Err(retry) => Step::Stay(WizardMode::PatInput(retry)),
                },
                KeyCode::Char(c) => {
                    w.push(c);
                    Step::Stay(WizardMode::PatInput(w))
                }
                _ => Step::Stay(WizardMode::PatInput(w)),
            },
            WizardMode::Confirmed(w) => match code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    Step::Stay(WizardMode::Done(w.write(&ctx.target)))
                }
                KeyCode::Esc | KeyCode::Char('n') => Step::Close(false),
                _ => Step::Stay(WizardMode::Confirmed(w)),
            },
            // Any key dismisses the result; reload iff the write succeeded.
            WizardMode::Done(w) => Step::Close(w.state.ok),
        }
    }
}

/// Render the centered wizard popup over the dashboard.
pub(crate) fn draw(f: &mut Frame, mode: &WizardMode) {
    let area = super::views::centered_rect(60, 40, f.area());
    f.render_widget(Clear, area);

    let (title, lines) = match mode {
        WizardMode::PickAction(_) => (
            " Configure ",
            vec![
                Line::from(""),
                Line::from("  [a]  add org + read-only PAT"),
                Line::from(Span::styled(
                    "  (more actions from the Config tab: [h] hooks · [m] metrics · [i] intervals)",
                    Style::new().fg(Color::DarkGray),
                )),
                Line::from(""),
                footer("[a] add · [Esc] close"),
            ],
        ),
        WizardMode::OrgInput(w) => (
            " Add org ",
            vec![
                Line::from(""),
                field("GitHub org login", &w.state.org),
                Line::from(""),
                footer("[Enter] next · [Esc] cancel"),
            ],
        ),
        WizardMode::PatInput(w) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::raw("  org  "),
                    Span::styled(&w.state.org, Style::new().add_modifier(Modifier::BOLD)),
                ]),
                field(
                    "Fine-grained PAT (github_pat_…)",
                    &"•".repeat(w.state.pat.chars().count()),
                ),
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

/// A labelled input field line with a trailing cursor block (only the currently
/// active field is ever rendered, so the cursor is unconditional).
fn field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::new().fg(Color::Gray)),
        Span::styled(
            format!("{value}▏"),
            Style::new().add_modifier(Modifier::BOLD),
        ),
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
    use super::*;

    // The typestate guarantee, exercised: PickAction → add → OrgInput → (type +
    // next) → PatInput. `write` is unreachable without a `Confirmed`, which only
    // `validate` (network) yields — so it is not exercised here, which is the
    // point: there is no other constructor.
    #[test]
    fn org_then_pat_flow_without_network() {
        let ctx = WizardCtx {
            local_ids: HashSet::new(),
            target: PathBuf::from("/tmp/x"),
        };
        let mut mode = WizardMode::new();
        mode = step(mode, KeyCode::Char('a'), &ctx); // → OrgInput
        assert!(matches!(mode, WizardMode::OrgInput(_)));
        for c in "acme".chars() {
            mode = step(mode, KeyCode::Char(c), &ctx);
        }
        mode = step(mode, KeyCode::Enter, &ctx); // → PatInput
        assert!(matches!(mode, WizardMode::PatInput(_)));
        // Esc closes without a change.
        assert!(matches!(
            mode.on_key(KeyCode::Esc, &ctx),
            Step::Close(false)
        ));
    }

    #[test]
    fn empty_org_cannot_advance() {
        let ctx = WizardCtx {
            local_ids: HashSet::new(),
            target: PathBuf::from("/tmp/x"),
        };
        let mode = step(WizardMode::new(), KeyCode::Char('a'), &ctx);
        // Enter with an empty org stays in OrgInput.
        let mode = step(mode, KeyCode::Enter, &ctx);
        assert!(matches!(mode, WizardMode::OrgInput(_)));
    }

    fn step(mode: WizardMode, code: KeyCode, ctx: &WizardCtx) -> WizardMode {
        match mode.on_key(code, ctx) {
            Step::Stay(m) => m,
            Step::Close(_) => panic!("unexpected close"),
        }
    }

    /// Render a wizard state into an in-memory `TestBackend` and flatten it to
    /// text — the deterministic, CI-able answer to "does it draw right?" (the
    /// insta approach this codebase uses instead of eyeballed tmux captures).
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
                pat: "github_pat_SUPERSECRETVALUE".to_string(),
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
                org: "example-org".to_string(),
            },
        });
        insta::assert_snapshot!(render(&mode));
    }

    #[test]
    fn snapshot_pat_input_with_rejection() {
        let mode = WizardMode::PatInput(Wizard {
            state: PatInput {
                org: "example-org".to_string(),
                pat: "github_pat_abcd".to_string(),
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
}
