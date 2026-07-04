//! Native in-TUI config wizard — a typestate popup. No CLI escape, no terminal
//! teardown: configuration happens *in* the dashboard.
//!
//! The compile-time contract (the whole reason this is safe): `write` exists
//! ONLY on `Wizard<Confirmed>`, and a `Confirmed` is reachable ONLY from a
//! successful `Wizard<PatInput>::validate`. So a rejected or un-validated PAT
//! can never be persisted — it does not compile. The PAT is rendered masked
//! (`•`) and never logged.
//!
//! Text editing is delegated to [`tui_input::Input`] (the ratatui-ecosystem
//! input widget), so cursor movement, insert/delete, and Home/End come for free
//! — the wizard only intercepts Enter/Esc for navigation.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::shared::github::validate::{self, Verdict};

/// What the wizard needs from the app to act: the locally-discovered runner
/// agentIds, for the agentId-confirm. The *how* of persisting is injected as a
/// `save` closure at key-time (see [`WizardMode::on_key`]), so the wizard is
/// agnostic to whether the token lands via the root collector (IPC) or a direct
/// config write.
pub(crate) struct WizardCtx {
    pub local_ids: HashSet<i64>,
}

/// The persist operation a committed wizard asks of its injected sink. One sink
/// handles both add/replace and remove, so the caller borrows its source (the
/// IPC client / config file) exactly once — two separate closures would each need
/// `&mut source` and could not coexist.
pub(crate) enum TokenOp<'a> {
    Set { org: &'a str, token: &'a str },
    Remove { org: &'a str },
}

// ---- typestate states (data-carrying; private fields ⇒ un-fabricable) ----

pub(crate) struct PickAction;
pub(crate) struct OrgInput {
    org: Input,
}
pub(crate) struct PatInput {
    org: String,
    pat: Input,
    error: Option<String>,
}
pub(crate) struct Confirmed {
    org: String,
    pat: String,
    matched: usize,
    local: usize,
}
/// Type the org whose PAT to remove (the `[r]` flow).
pub(crate) struct RemoveOrgInput {
    org: Input,
}
/// Confirm removing `org`'s PAT (and forgetting the org).
pub(crate) struct RemoveConfirm {
    org: String,
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
            state: OrgInput {
                org: Input::default(),
            },
        }
    }
    fn remove_org(self) -> Wizard<RemoveOrgInput> {
        Wizard {
            state: RemoveOrgInput {
                org: Input::default(),
            },
        }
    }
}

/// The two next-states from the remove-org field (mirrors [`OrgNext`]).
enum RemoveNext {
    Confirm(Wizard<RemoveConfirm>),
    Stay(Wizard<RemoveOrgInput>),
}

impl Wizard<RemoveOrgInput> {
    fn edit(&mut self, key: KeyEvent) {
        self.state.org.handle_event(&Event::Key(key));
    }
    /// Advance to the confirm step — only with a non-empty org (else stay put).
    fn next(self) -> RemoveNext {
        let org = self.state.org.value().trim().to_string();
        if org.is_empty() {
            return RemoveNext::Stay(self);
        }
        RemoveNext::Confirm(Wizard {
            state: RemoveConfirm { org },
        })
    }
}

impl Wizard<RemoveConfirm> {
    /// Remove the org's PAT via the injected sink (IPC to the collector, or a
    /// direct config write). No PAT to validate — removal is unconditional.
    fn write_remove(self, apply: impl FnOnce(TokenOp) -> Result<(), String>) -> Wizard<Done> {
        let done = match apply(TokenOp::Remove {
            org: &self.state.org,
        }) {
            Ok(()) => Done {
                message: format!("removed token and forgot org {}", self.state.org),
                ok: true,
            },
            Err(e) => Done {
                message: format!("remove failed: {e}"),
                ok: false,
            },
        };
        Wizard { state: done }
    }
}

/// The two next-states from the org field — both valid branches (not an error,
/// so not a `Result`; that also keeps the large `Wizard<PatInput>` off every
/// `Result`'s error path).
enum OrgNext {
    Pat(Wizard<PatInput>),
    Stay(Wizard<OrgInput>),
}

/// The two next-states from PAT validation: a validated `Confirmed`, or back to
/// `PatInput` for the SAME org with the rejection reason.
enum PatNext {
    Confirm(Wizard<Confirmed>),
    Reject(Wizard<PatInput>),
}

impl Wizard<OrgInput> {
    /// Delegate a key to the input widget (typing, backspace, cursor, …).
    fn edit(&mut self, key: KeyEvent) {
        self.state.org.handle_event(&Event::Key(key));
    }
    /// Advance to PAT entry — only with a non-empty org (else stay put).
    fn next(self) -> OrgNext {
        let org = self.state.org.value().trim().to_string();
        if org.is_empty() {
            return OrgNext::Stay(self);
        }
        OrgNext::Pat(Wizard {
            state: PatInput {
                org,
                pat: Input::default(),
                error: None,
            },
        })
    }
}

impl Wizard<PatInput> {
    fn edit(&mut self, key: KeyEvent) {
        self.state.pat.handle_event(&Event::Key(key));
    }
    /// Validate the PAT (sync `github::validate`). Valid ⇒ `Confirmed`; rejected
    /// ⇒ back to `PatInput` for THAT org, prefilled, PAT cleared, reason shown.
    fn validate(self, local_ids: &HashSet<i64>) -> PatNext {
        let pat = self.state.pat.value().to_string();
        match validate::validate(&pat, &self.state.org, local_ids) {
            Verdict::Valid { matched, local, .. } => PatNext::Confirm(Wizard {
                state: Confirmed {
                    org: self.state.org,
                    pat,
                    matched,
                    local,
                },
            }),
            Verdict::Rejected(why) => PatNext::Reject(Wizard {
                state: PatInput {
                    org: self.state.org,
                    pat: Input::default(),
                    error: Some(why),
                },
            }),
        }
    }
}

impl Wizard<Confirmed> {
    /// Persist the validated token via the injected `save` sink. The ONLY persist
    /// path — reachable only from a successful `validate`, so the typestate keeps
    /// its "validated-only persist" guarantee regardless of the sink (IPC to the
    /// root collector, or a direct config write). `save` returns `Err(msg)` with a
    /// human reason (unauthorized / write failed) that is surfaced in `Done`.
    fn write(self, apply: impl FnOnce(TokenOp) -> Result<(), String>) -> Wizard<Done> {
        let done = match apply(TokenOp::Set {
            org: &self.state.org,
            token: &self.state.pat,
        }) {
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
    RemoveOrgInput(Wizard<RemoveOrgInput>),
    RemoveConfirm(Wizard<RemoveConfirm>),
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

    /// Route one key. Consumes `self` (the typestate). The text states intercept
    /// Enter/Esc for navigation and hand every other key to the input widget;
    /// `validate`/`write` block briefly (a sync network call / a persist round-trip).
    /// `save` is the injected persist sink, invoked only when a `Confirmed` wizard
    /// is committed — the App routes it through the root collector (IPC) with a
    /// direct-write fallback.
    pub(crate) fn on_key(
        self,
        key: KeyEvent,
        ctx: &WizardCtx,
        apply: impl FnOnce(TokenOp) -> Result<(), String>,
    ) -> Step {
        match self {
            WizardMode::PickAction(w) => match key.code {
                KeyCode::Char('a') => Step::Stay(WizardMode::OrgInput(w.add_org())),
                KeyCode::Char('r') => Step::Stay(WizardMode::RemoveOrgInput(w.remove_org())),
                KeyCode::Esc => Step::Close(false),
                _ => Step::Stay(WizardMode::PickAction(w)),
            },
            WizardMode::OrgInput(mut w) => match key.code {
                KeyCode::Esc => Step::Close(false),
                KeyCode::Enter => match w.next() {
                    OrgNext::Pat(next) => Step::Stay(WizardMode::PatInput(next)),
                    OrgNext::Stay(same) => Step::Stay(WizardMode::OrgInput(same)),
                },
                _ => {
                    w.edit(key);
                    Step::Stay(WizardMode::OrgInput(w))
                }
            },
            WizardMode::PatInput(mut w) => match key.code {
                KeyCode::Esc => Step::Close(false),
                KeyCode::Enter => match w.validate(&ctx.local_ids) {
                    PatNext::Confirm(confirmed) => Step::Stay(WizardMode::Confirmed(confirmed)),
                    PatNext::Reject(retry) => Step::Stay(WizardMode::PatInput(retry)),
                },
                _ => {
                    w.edit(key);
                    Step::Stay(WizardMode::PatInput(w))
                }
            },
            WizardMode::Confirmed(w) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Step::Stay(WizardMode::Done(w.write(apply))),
                KeyCode::Esc | KeyCode::Char('n') => Step::Close(false),
                _ => Step::Stay(WizardMode::Confirmed(w)),
            },
            WizardMode::RemoveOrgInput(mut w) => match key.code {
                KeyCode::Esc => Step::Close(false),
                KeyCode::Enter => match w.next() {
                    RemoveNext::Confirm(next) => Step::Stay(WizardMode::RemoveConfirm(next)),
                    RemoveNext::Stay(same) => Step::Stay(WizardMode::RemoveOrgInput(same)),
                },
                _ => {
                    w.edit(key);
                    Step::Stay(WizardMode::RemoveOrgInput(w))
                }
            },
            WizardMode::RemoveConfirm(w) => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    Step::Stay(WizardMode::Done(w.write_remove(apply)))
                }
                KeyCode::Esc | KeyCode::Char('n') => Step::Close(false),
                _ => Step::Stay(WizardMode::RemoveConfirm(w)),
            },
            // Any key dismisses the result; reload iff the write succeeded.
            WizardMode::Done(w) => Step::Close(w.state.ok),
        }
    }
}

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
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn ev(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // The typestate guarantee, exercised: PickAction → add → OrgInput → (type +
    // next) → PatInput. `write` is unreachable without a `Confirmed`, which only
    // `validate` (network) yields — so it is not exercised here, which is the
    // point: there is no other constructor.
    #[test]
    fn org_then_pat_flow_without_network() {
        let ctx = WizardCtx {
            local_ids: HashSet::new(),
        };
        let mut mode = WizardMode::new();
        mode = step(mode, ev(KeyCode::Char('a')), &ctx); // → OrgInput
        assert!(matches!(mode, WizardMode::OrgInput(_)));
        for c in "acme".chars() {
            mode = step(mode, ev(KeyCode::Char(c)), &ctx);
        }
        mode = step(mode, ev(KeyCode::Enter), &ctx); // → PatInput
        assert!(matches!(mode, WizardMode::PatInput(_)));
        // Esc closes without a change.
        assert!(matches!(
            mode.on_key(ev(KeyCode::Esc), &ctx, no_apply),
            Step::Close(false)
        ));
    }

    // The remove flow: PickAction → [r] → RemoveOrgInput → (type + Enter) →
    // RemoveConfirm → [y] commits via the remove sink and reloads on success.
    #[test]
    fn remove_org_flow_confirms_and_saves() {
        let ctx = WizardCtx {
            local_ids: HashSet::new(),
        };
        let mut mode = step(WizardMode::new(), ev(KeyCode::Char('r')), &ctx);
        assert!(matches!(mode, WizardMode::RemoveOrgInput(_)));
        // Empty org can't advance.
        mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::RemoveOrgInput(_)));
        for c in "acme".chars() {
            mode = step(mode, ev(KeyCode::Char(c)), &ctx);
        }
        mode = step(mode, ev(KeyCode::Enter), &ctx); // → RemoveConfirm
        assert!(matches!(mode, WizardMode::RemoveConfirm(_)));
        // [y] runs the remove op and advances to Done.
        let removed = std::cell::Cell::new(None);
        let apply = |op: TokenOp| {
            if let TokenOp::Remove { org } = op {
                removed.set(Some(org.to_string()));
            }
            Ok(())
        };
        assert!(matches!(
            mode.on_key(ev(KeyCode::Char('y')), &ctx, apply),
            Step::Stay(WizardMode::Done(_))
        ));
        assert_eq!(removed.into_inner().as_deref(), Some("acme"));
    }

    #[test]
    fn empty_org_cannot_advance() {
        let ctx = WizardCtx {
            local_ids: HashSet::new(),
        };
        let mode = step(WizardMode::new(), ev(KeyCode::Char('a')), &ctx);
        // Enter with an empty org stays in OrgInput.
        let mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::OrgInput(_)));
    }

    /// A no-op persist sink for the navigation tests — most never reach a commit
    /// state (which needs a live `validate`), so it is usually never invoked.
    fn no_apply(_op: TokenOp) -> Result<(), String> {
        Ok(())
    }

    fn step(mode: WizardMode, key: KeyEvent, ctx: &WizardCtx) -> WizardMode {
        match mode.on_key(key, ctx, no_apply) {
            Step::Stay(m) => m,
            Step::Close(_) => panic!("unexpected close"),
        }
    }

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
