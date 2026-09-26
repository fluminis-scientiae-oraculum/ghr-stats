//! Synchronous, read-only dashboard: a live sampler, plus an IPC reader of the
//! collector in Persistent mode. The loop owns terminal teardown for suspended actions.

mod app;
mod input;
mod overlay;
mod source;
mod view;
mod viewmodel;

use std::io::stdout;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseEvent,
};
use ratatui::crossterm::execute;
use ratatui::{DefaultTerminal, Frame};

use app::{App, Overlay, Tab};
use input::action::{ActionKind, InstallHooks, OpenConfig};
use input::screen::{Confirm, Screen, ScreenState, Suspension};

use crate::shared::config::Config;

const REFRESH: Duration = Duration::from_millis(2000);

/// `config_path` is the `--config` override, so the wizard writes back to the loaded file.
pub fn run(cfg: &Config, config_path: Option<&Path>) -> Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mouse = MouseCapture::enable();
    let result = event_loop(&mut terminal, cfg, config_path);
    drop(mouse); // before ratatui leaves the alternate screen
    ratatui::restore();
    result
}

/// `ratatui::init`'s panic hook restores raw mode and the alternate screen but not
/// mouse capture; disabling it in `Drop` covers unwind too.
struct MouseCapture;

impl MouseCapture {
    fn enable() -> Self {
        let _ = execute!(stdout(), EnableMouseCapture);
        MouseCapture
    }
}

impl Drop for MouseCapture {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture);
    }
}

enum Next {
    Mode(ScreenState),
    Execute(Screen<Confirm>),
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
            let next = match event::read()? {
                // An open overlay captures every key before the screen state machine.
                Event::Key(k) if k.kind == KeyEventKind::Press && app.overlay_open() => {
                    app.overlay_key(k);
                    Next::Mode(mode)
                }
                Event::Key(k) if k.kind == KeyEventKind::Press => route_key(mode, &mut app, k.code),
                // An overlay is modal: a click must not act on the dashboard under it.
                Event::Mouse(_) if app.overlay_open() => Next::Mode(mode),
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
                        if crate::shared::privileged::is_root() {
                            let action = ActionKind::InstallHooks(InstallHooks {
                                roots: app.cfg().runner_roots.clone(),
                            });
                            return Next::Mode(ScreenState::Confirm(scr.confirm(action)));
                        }
                        app.open_info("Hook install needs root", overlay::help::root_guidance());
                        return Next::Mode(ScreenState::Browsing(scr));
                    }
                    _ => {}
                }
            }
            if app.drill.is_some() {
                let armed = match code {
                    KeyCode::Char('R') => Some(app.restart_action()),
                    KeyCode::Char('C') => Some(app.recycle_action()),
                    _ => None,
                };
                match armed {
                    Some(Ok(a)) => return Next::Mode(ScreenState::Confirm(scr.confirm(a))),
                    Some(Err(why)) => {
                        app.status = Some(format!("✗ {why}"));
                        return Next::Mode(ScreenState::Browsing(scr));
                    }
                    None => {}
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
    if !matches!(mode, ScreenState::Browsing(_)) {
        return Next::Mode(mode);
    }
    match app.on_mouse(m) {
        Some(code) => route_key(mode, app, code),
        None => Next::Mode(mode),
    }
}

/// Run the action on the real TTY; `Suspension` restores the terminal on every error path.
fn run_suspended(
    confirm: Screen<Confirm>,
    app: &mut App,
    terminal: &mut DefaultTerminal,
) -> Result<ScreenState> {
    let (guard, torn, mut tty) = Suspension::enter(terminal)?;
    let suspended = confirm.suspend(&torn);
    let outcome = suspended.execute(&mut tty);
    let restored = guard.resume()?;
    let browsing = suspended.resume(restored);
    app.status = Some(outcome.message());
    Ok(ScreenState::Browsing(browsing))
}

fn render(f: &mut Frame, app: &App, mode: &ScreenState) {
    view::draw(f, app);
    if let ScreenState::Confirm(scr) = mode {
        view::draw_confirm(f, &scr.prompt());
    }
    match app.overlay() {
        Some(Overlay::Wizard(w)) => overlay::wizard::draw(f, w),
        Some(Overlay::Help) => overlay::help::draw_help(f),
        Some(Overlay::Info { title, body }) => overlay::help::draw_info(f, title, body),
        None => {}
    }
}
