use ratatui::style::Color;

use crate::shared::models::Mode;

pub(crate) fn mode_badge(mode: Mode) -> (&'static str, Color) {
    match mode {
        Mode::Persistent => ("PERSISTENT", Color::Green),
        Mode::Ephemeral => ("EPHEMERAL", Color::Yellow),
    }
}

pub(crate) fn mode_word(mode: Mode) -> (&'static str, Color) {
    match mode {
        Mode::Persistent => ("Persistent", Color::Green),
        Mode::Ephemeral => ("Ephemeral", Color::Yellow),
    }
}
