//! Interaction typestate. The proof tokens `Torn`/`Restored`/`Tty` are minted only by
//! [`Suspension`], so an action cannot run outside a suspend window.

use std::io::{self, stdout};

use ratatui::DefaultTerminal;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::tui::input::action::{ActionKind, ActionOutcome, ConfirmPrompt};

pub(crate) struct Torn(());
pub(crate) struct Restored(());
/// The real TTY is in cooked mode, so a child can inherit stdio.
pub(crate) struct Tty(());

pub(crate) struct Browsing;
pub(crate) struct Confirm {
    pending: ActionKind,
}
pub(crate) struct Suspended {
    pending: ActionKind,
}

pub(crate) struct Screen<S> {
    state: S,
}

impl Screen<Browsing> {
    pub(crate) fn new() -> Self {
        Screen { state: Browsing }
    }

    pub(crate) fn confirm(self, action: ActionKind) -> Screen<Confirm> {
        Screen {
            state: Confirm { pending: action },
        }
    }
}

impl Screen<Confirm> {
    pub(crate) fn prompt(&self) -> ConfirmPrompt {
        self.state.pending.prompt()
    }

    pub(crate) fn cancel(self) -> Screen<Browsing> {
        Screen { state: Browsing }
    }

    pub(crate) fn suspend(self, _torn: &Torn) -> Screen<Suspended> {
        Screen {
            state: Suspended {
                pending: self.state.pending,
            },
        }
    }
}

impl Screen<Suspended> {
    pub(crate) fn execute(&self, tty: &mut Tty) -> ActionOutcome {
        self.state.pending.execute(tty)
    }

    pub(crate) fn resume(self, _restored: Restored) -> Screen<Browsing> {
        Screen { state: Browsing }
    }
}

pub(crate) enum ScreenState {
    Browsing(Screen<Browsing>),
    Confirm(Screen<Confirm>),
}

impl ScreenState {
    pub(crate) fn browsing() -> Self {
        ScreenState::Browsing(Screen::new())
    }
}

/// `Drop` restores on error paths; crossterm's toggles are idempotent and
/// `ratatui::init`'s panic hook stays installed throughout.
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
