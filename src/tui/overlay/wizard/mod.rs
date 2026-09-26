//! In-TUI config wizard typestate. `write` exists only on `Wizard<Confirmed>`, reachable
//! only from a successful `validate`, so an unvalidated PAT cannot be persisted.

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::shared::github::RunnerScope;
use crate::shared::github::validate::{self, PatCheck};

mod draw;

pub(crate) use draw::draw;

pub(crate) struct WizardCtx {
    /// This host's runners as `(scope, agentId)`.
    pub local: Vec<(RunnerScope, i64)>,
}

pub(crate) enum TokenOp<'a> {
    Set { org: &'a str, token: &'a str },
    Remove { org: &'a str },
}

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
pub(crate) struct RemoveOrgInput {
    org: Input,
}
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

enum RemoveNext {
    Confirm(Wizard<RemoveConfirm>),
    Stay(Wizard<RemoveOrgInput>),
}

impl Wizard<RemoveOrgInput> {
    fn edit(&mut self, key: KeyEvent) {
        self.state.org.handle_event(&Event::Key(key));
    }
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

/// Not a `Result`: staying put is not an error.
enum OrgNext {
    Pat(Wizard<PatInput>),
    Stay(Wizard<OrgInput>),
}

enum PatNext {
    Confirm(Wizard<Confirmed>),
    Reject(Wizard<PatInput>),
}

impl Wizard<OrgInput> {
    fn edit(&mut self, key: KeyEvent) {
        self.state.org.handle_event(&Event::Key(key));
    }
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
    fn validate(self, local: &[(RunnerScope, i64)]) -> PatNext {
        let pat = self.state.pat.value().to_string();
        match validate::validate(&pat, &self.state.org, local) {
            PatCheck::Valid { matched, local, .. } => PatNext::Confirm(Wizard {
                state: Confirmed {
                    org: self.state.org,
                    pat,
                    matched,
                    local,
                },
            }),
            PatCheck::Rejected(why) => PatNext::Reject(Wizard {
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

pub(crate) enum WizardMode {
    PickAction(Wizard<PickAction>),
    OrgInput(Wizard<OrgInput>),
    PatInput(Wizard<PatInput>),
    Confirmed(Wizard<Confirmed>),
    RemoveOrgInput(Wizard<RemoveOrgInput>),
    RemoveConfirm(Wizard<RemoveConfirm>),
    Done(Wizard<Done>),
}

pub(crate) enum Step {
    Stay(WizardMode),
    /// `true` if the config changed.
    Close(bool),
}

impl WizardMode {
    pub(crate) fn new() -> Self {
        WizardMode::PickAction(Wizard { state: PickAction })
    }

    /// Blocks during `validate` (network call) and `apply`.
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
                KeyCode::Enter => match w.validate(&ctx.local) {
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

    #[test]
    fn org_then_pat_flow_without_network() {
        let ctx = WizardCtx { local: Vec::new() };
        let mut mode = WizardMode::new();
        mode = step(mode, ev(KeyCode::Char('a')), &ctx);
        assert!(matches!(mode, WizardMode::OrgInput(_)));
        for c in "acme".chars() {
            mode = step(mode, ev(KeyCode::Char(c)), &ctx);
        }
        mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::PatInput(_)));
        assert!(matches!(
            mode.on_key(ev(KeyCode::Esc), &ctx, no_apply),
            Step::Close(false)
        ));
    }

    #[test]
    fn remove_org_flow_confirms_and_saves() {
        let ctx = WizardCtx { local: Vec::new() };
        let mut mode = step(WizardMode::new(), ev(KeyCode::Char('r')), &ctx);
        assert!(matches!(mode, WizardMode::RemoveOrgInput(_)));
        mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::RemoveOrgInput(_)));
        for c in "acme".chars() {
            mode = step(mode, ev(KeyCode::Char(c)), &ctx);
        }
        mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::RemoveConfirm(_)));
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
        let ctx = WizardCtx { local: Vec::new() };
        let mode = step(WizardMode::new(), ev(KeyCode::Char('a')), &ctx);
        let mode = step(mode, ev(KeyCode::Enter), &ctx);
        assert!(matches!(mode, WizardMode::OrgInput(_)));
    }

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
