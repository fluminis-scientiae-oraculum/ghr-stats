//! Typestate interaction core — the interaction *mode*, kept separate from the
//! data (`App`). Invalid transitions don't compile because the methods that
//! would perform them only exist on the right state type:
//!
//! - `execute`/`resume` exist ONLY on `Screen<Suspended<A>>`.
//! - The ONLY way to a `Suspended` is `Confirm::suspend(&Torn)`.
//! - The ONLY way to a `Confirm` is `Browsing::confirm(action)`.
//! - `Torn`/`Restored`/`Tty` are ZSTs minted ONLY by `Suspension`, which tears
//!   the terminal down — so an action can't run outside a suspend window.
//!
//! ```ignore
//! let b = Screen::<Browsing>::new();
//! b.execute(&mut tty);          // E0599: no method `execute` on Screen<Browsing>
//! let c = b.confirm(action);
//! c.resume(restored);           // E0599: no method `resume` on Screen<Confirm<_>>
//! c.suspend(&Torn(()));         // E0451: field of `Torn` is private (mint via Suspension)
//! ```

use std::io::{self, stdout};

use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::tui::input::action::{ActionKind, ActionOutcome, ConfirmPrompt};

// --- proof tokens: ZSTs with a private field, minted only by `Suspension` ---

/// Proof the terminal was torn down (raw off, alt screen left).
pub(crate) struct Torn(());
/// Proof the terminal was re-initialised.
pub(crate) struct Restored(());
/// Capability: the real TTY is in cooked mode (a child can inherit stdio).
pub(crate) struct Tty(());

// --- marker states (private fields ⇒ un-fabricable from outside) ---

pub(crate) struct Browsing;
pub(crate) struct Confirm {
    pending: ActionKind,
}
pub(crate) struct Suspended {
    pending: ActionKind,
}

/// The interaction mode. `S` is the state marker.
pub(crate) struct Screen<S> {
    state: S,
}

impl Screen<Browsing> {
    pub(crate) fn new() -> Self {
        Screen { state: Browsing }
    }

    /// The ONLY constructor of a pending action.
    pub(crate) fn confirm(self, action: ActionKind) -> Screen<Confirm> {
        Screen {
            state: Confirm { pending: action },
        }
    }
}

impl Screen<Confirm> {
    /// The prompt to render — proof a pending action exists.
    pub(crate) fn prompt(&self) -> ConfirmPrompt {
        self.state.pending.prompt()
    }

    /// User declined: back to browsing, no TTY work.
    pub(crate) fn cancel(self) -> Screen<Browsing> {
        Screen { state: Browsing }
    }

    /// Move to `Suspended` — requires proof the terminal was torn down.
    pub(crate) fn suspend(self, _torn: &Torn) -> Screen<Suspended> {
        Screen {
            state: Suspended {
                pending: self.state.pending,
            },
        }
    }
}

impl Screen<Suspended> {
    /// Run the action on the real TTY (the `Tty` token proves we are suspended).
    pub(crate) fn execute(&self, tty: &mut Tty) -> ActionOutcome {
        self.state.pending.execute(tty)
    }

    /// Back to browsing — requires proof the terminal was re-initialised.
    pub(crate) fn resume(self, _restored: Restored) -> Screen<Browsing> {
        Screen { state: Browsing }
    }
}

/// The runtime dispatch enum the event loop owns (the typestate changes type on
/// each transition, but a loop needs one fixed type). Per-state methods stay
/// compile-time-guarded regardless of this erasure.
pub(crate) enum ScreenState {
    Browsing(Screen<Browsing>),
    Confirm(Screen<Confirm>),
}

impl ScreenState {
    pub(crate) fn browsing() -> Self {
        ScreenState::Browsing(Screen::new())
    }
}

/// RAII guard coupling terminal teardown to the typestate transition. `enter`
/// tears the terminal down and mints the proof tokens; `resume` re-initialises
/// it; `Drop` is the error-path backstop. The panic hook installed once by
/// `ratatui::init` stays active throughout, and crossterm's toggles are
/// idempotent, so a panic mid-action still lands the terminal in a sane state.
pub(crate) struct Suspension<'t> {
    term: &'t mut DefaultTerminal,
    restored: bool,
}

impl<'t> Suspension<'t> {
    pub(crate) fn enter(term: &'t mut DefaultTerminal) -> io::Result<(Self, Torn, Tty)> {
        disable_raw_mode()?;
        execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture, Show)?;
        Ok((
            Self {
                term,
                restored: false,
            },
            Torn(()),
            Tty(()),
        ))
    }

    pub(crate) fn resume(mut self) -> io::Result<Restored> {
        self.restore()?;
        Ok(Restored(()))
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        self.term.clear()?; // ratatui repaints from scratch next frame
        self.restored = true;
        Ok(())
    }
}

impl Drop for Suspension<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}
