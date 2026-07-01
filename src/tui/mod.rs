//! Interactive dashboard. Fully synchronous — blocking terminal I/O, called
//! directly from `main`. The dashboard never writes: it is a pure client — an
//! in-memory live sampler always, plus (in Persistent mode) an IPC reader of the
//! collector (see `app` + `history`).
//!
//! Interaction is a typestate (see `screen`): the loop owns a runtime
//! `ScreenState`, routes each event through it, and owns terminal teardown for
//! the suspend window an action needs.

mod action;
mod app;
mod help;
mod history;
mod screen;
mod views;
mod wizard;

use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEvent,
};
use ratatui::crossterm::execute;
use ratatui::{DefaultTerminal, Frame};

use action::{ActionKind, InstallHooks, OpenConfig};
use app::{App, Overlay, Tab};
use screen::{Confirm, Screen, ScreenState, Suspension};

use crate::config::Config;

/// Live view refresh cadence (the loop still redraws immediately on input).
const REFRESH: Duration = Duration::from_millis(2000);

/// Set up the terminal, run the event loop, and always restore on exit.
/// `ratatui::init` also installs a panic hook that restores the terminal.
/// `config_path` is the resolved `--config` override (if any) — threaded so the
/// native config wizard writes back to the same file the run loaded.
pub fn run(cfg: &Config, config_path: Option<&Path>) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, cfg, config_path);
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

fn event_loop(
    terminal: &mut DefaultTerminal,
    cfg: &Config,
    config_path: Option<&Path>,
) -> Result<()> {
    let mut app = App::new(cfg.clone(), config_path.map(Path::to_path_buf));
    app.refresh();
    let mut mode = ScreenState::browsing();
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|f| render(f, &app, &mode))?;

        let timeout = REFRESH.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            // Every arm moves `mode` into `next`; `mode` is reassigned by `drive`.
            let next = match event::read()? {
                // A modal overlay (wizard / help / info) captures every key while
                // open, so it is routed BEFORE the browsing/confirm state machine.
                // Overlays never suspend the terminal.
                Event::Key(k) if k.kind == KeyEventKind::Press && app.overlay_open() => {
                    app.overlay_key(k);
                    Next::Mode(mode)
                }
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
            // Config-tab actions. `[a]` add-org and `[m]` metrics are native (no
            // teardown, loop stays Browsing); `[o]` open-config and `[h]` hooks
            // are suspend-to-TTY actions (arm Confirm → suspend). `[h]` is
            // root-gated INFORMATIONALLY: non-root shows guidance, never an error.
            if app.tab == Tab::Config && app.drill.is_none() {
                match code {
                    KeyCode::Char('a') => {
                        app.open_wizard();
                        return Next::Mode(ScreenState::Browsing(scr));
                    }
                    KeyCode::Char('m') => {
                        app.toggle_metrics();
                        return Next::Mode(ScreenState::Browsing(scr));
                    }
                    KeyCode::Char('o') => {
                        let action = ActionKind::OpenConfig(OpenConfig {
                            path: app.config_target(),
                        });
                        return Next::Mode(ScreenState::Confirm(scr.confirm(action)));
                    }
                    KeyCode::Char('h') => {
                        if crate::privileged::is_root() {
                            let action = ActionKind::InstallHooks(InstallHooks {
                                roots: app.cfg().runner_roots.clone(),
                            });
                            return Next::Mode(ScreenState::Confirm(scr.confirm(action)));
                        }
                        app.open_info(
                            "Hook install needs root",
                            crate::privileged::root_guidance(),
                        );
                        return Next::Mode(ScreenState::Browsing(scr));
                    }
                    _ => {}
                }
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
    // The modal overlay is drawn last so it sits atop the dashboard + any confirm
    // popup. (In practice they are mutually exclusive — overlays open only from
    // Browsing.)
    match app.overlay() {
        Some(Overlay::Wizard(w)) => wizard::draw(f, w),
        Some(Overlay::Help) => help::draw_help(f),
        Some(Overlay::Info { title, body }) => help::draw_info(f, title, body),
        None => {}
    }
}
