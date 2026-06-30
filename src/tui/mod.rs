//! Interactive dashboard. Fully synchronous — blocking terminal I/O + DB reads,
//! called directly from `main`. The dashboard is a PURE reader (see `app`).
//!
//! Interaction is a typestate (see `screen`): the loop owns a runtime
//! `ScreenState`, routes each event through it, and owns terminal teardown for
//! the suspend window an action needs.

mod action;
mod app;
mod screen;
mod views;

use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEvent,
};
use ratatui::crossterm::execute;
use ratatui::{DefaultTerminal, Frame};

use action::{ActionKind, AddOrg};
use app::{App, Tab};
use screen::{Confirm, Screen, ScreenState, Suspension};

use crate::config::Config;

/// Live view refresh cadence (the loop still redraws immediately on input).
const REFRESH: Duration = Duration::from_millis(2000);

/// Set up the terminal, run the event loop, and always restore on exit.
/// `ratatui::init` also installs a panic hook that restores the terminal.
pub fn run(cfg: &Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, cfg);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// What an input handler decides the loop should do next.
enum Next {
    /// Stay in the TUI in this mode.
    Mode(ScreenState),
    /// The user accepted: the loop must suspend, run the action, and resume.
    Execute(Screen<Confirm<ActionKind>>),
}

fn event_loop(terminal: &mut DefaultTerminal, cfg: &Config) -> Result<()> {
    let mut app = App::new(cfg.clone());
    app.refresh();
    let mut mode = ScreenState::browsing();
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|f| render(f, &app, &mode))?;

        let timeout = REFRESH.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            // Every arm moves `mode` into `next`; `mode` is reassigned by `drive`.
            let next = match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => route_key(mode, &mut app, k.code),
                Event::Mouse(m) => route_mouse(mode, &mut app, m),
                _ => Next::Mode(mode),
            };
            mode = drive(next, &mut app, terminal)?;
        }

        if last_tick.elapsed() >= REFRESH {
            app.refresh();
            last_tick = Instant::now();
        }
    }
    Ok(())
}

fn drive(next: Next, app: &mut App, terminal: &mut DefaultTerminal) -> Result<ScreenState> {
    match next {
        Next::Mode(m) => Ok(m),
        Next::Execute(confirm) => run_suspended(confirm, app, terminal),
    }
}

fn route_key(mode: ScreenState, app: &mut App, code: KeyCode) -> Next {
    match mode {
        ScreenState::Browsing(scr) => {
            // Action triggers are armed here (Browsing -> Confirm).
            // Config tab: 'a' arms the config wizard.
            if app.tab == Tab::Config && app.drill.is_none() && code == KeyCode::Char('a') {
                return Next::Mode(ScreenState::Confirm(
                    scr.confirm(ActionKind::AddOrg(AddOrg)),
                ));
            }
            // Detail drill-down: R = restart, C = recycle (idle-only) the runner.
            if app.drill.is_some() {
                let armed = match code {
                    KeyCode::Char('R') => app.restart_action(),
                    KeyCode::Char('C') => app.recycle_action(),
                    _ => None,
                };
                if let Some(a) = armed {
                    return Next::Mode(ScreenState::Confirm(scr.confirm(a)));
                }
            }
            app.on_key(code);
            Next::Mode(ScreenState::Browsing(scr))
        }
        ScreenState::Confirm(scr) => match code {
            KeyCode::Char('y') | KeyCode::Enter => Next::Execute(scr),
            KeyCode::Char('n') | KeyCode::Esc => Next::Mode(ScreenState::Browsing(scr.cancel())),
            _ => Next::Mode(ScreenState::Confirm(scr)),
        },
    }
}

fn route_mouse(mode: ScreenState, app: &mut App, m: MouseEvent) -> Next {
    // Only browsing handles mouse (tab clicks / scroll); confirm/suspended ignore.
    if let ScreenState::Browsing(_) = mode {
        app.on_mouse(m);
    }
    Next::Mode(mode)
}

/// Suspend ratatui, run the action on the real TTY, resume. Only the loop can do
/// this — it owns the terminal. The `Suspension` guard couples teardown to the
/// typestate transition via proof tokens and restores on any error path.
fn run_suspended(
    confirm: Screen<Confirm<ActionKind>>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
) -> Result<ScreenState> {
    let (guard, torn, mut tty) = Suspension::enter(terminal)?;
    let suspended = confirm.suspend(&torn); // Confirm -> Suspended (needs &Torn)
    let outcome = suspended.execute(&mut tty); // sudo/wizard on the real TTY
    let restored = guard.resume()?; // ratatui re-initialised
    let browsing = suspended.resume(restored); // Suspended -> Browsing (needs Restored)
    app.status = Some(outcome.message());
    Ok(ScreenState::Browsing(browsing))
}

fn render(f: &mut Frame, app: &App, mode: &ScreenState) {
    views::draw(f, app);
    if let ScreenState::Confirm(scr) = mode {
        views::draw_confirm(f, &scr.prompt());
    }
}
