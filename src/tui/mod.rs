//! Interactive dashboard. Fully synchronous — blocking `/proc`/`/sys` polls +
//! blocking terminal I/O — called directly from `main`.

mod app;
mod views;

use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::config::Config;
use app::App;

/// Live view refresh cadence (the loop still redraws immediately on input).
const REFRESH: Duration = Duration::from_millis(2000);

/// Set up the terminal, run the event loop, and always restore on exit.
/// `ratatui::init` also installs a panic hook that restores the terminal.
pub fn run(cfg: &Config) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, cfg);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, cfg: &Config) -> Result<()> {
    let mut app = App::new(cfg.clone());
    app.refresh();
    let mut last_tick = Instant::now();

    while !app.should_quit {
        terminal.draw(|f| views::draw(f, &app))?;

        let timeout = REFRESH.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key.code);
        }

        if last_tick.elapsed() >= REFRESH {
            app.refresh();
            last_tick = Instant::now();
        }
    }
    Ok(())
}
