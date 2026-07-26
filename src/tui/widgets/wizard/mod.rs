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
//!
//! Cut into WHAT THE MACHINE IS and WHAT IT LOOKS LIKE:
//!
//! - **this file** — the typestate and the loop that drives it: the states with
//!   their un-fabricable private fields, the transitions that are the only way
//!   between them, and [`WizardMode`], which erases those types back into one
//!   runtime match because a loop needs a single fixed type.
//! - [`draw`] — what the operator sees, and the masking that keeps the PAT off
//!   the screen.
//!
//! Two files rather than three, and the import list is why. [`WizardMode::on_key`]
//! reads like a separate concern from the transitions it calls, but the two share
//! their whole vocabulary — `KeyEvent`, `Event`, `Input`, [`TokenOp`] — so a cut
//! between them would separate nothing. The render half is the opposite: every
//! ratatui drawing type in the module appears there and nowhere else, `draw` is
//! reached from one call site (`tui::mod`) while everything else is reached from
//! another (`tui::app::mutate`), and `263f490` and `888d653` each changed the
//! drawing alone.

use std::collections::HashSet;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::shared::github::validate::{self, Verdict};

mod draw;

pub(crate) use draw::draw;

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
}
