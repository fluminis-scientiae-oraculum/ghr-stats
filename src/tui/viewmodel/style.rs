//! Mode rendering — the single home for how a [`Mode`] is shown, so the badge
//! (header) and the Config tab can't drift apart on label or colour.

use ratatui::style::Color;

use crate::tui::history::Mode;

/// The header-badge label (uppercase) + colour.
pub(crate) fn mode_badge(mode: Mode) -> (&'static str, Color) {
    match mode {
        Mode::Persistent => ("PERSISTENT", Color::Green),
        Mode::Ephemeral => ("EPHEMERAL", Color::Yellow),
    }
}

/// The title-case word + colour for the Config tab's Mode line.
pub(crate) fn mode_word(mode: Mode) -> (&'static str, Color) {
    match mode {
        Mode::Persistent => ("Persistent", Color::Green),
        Mode::Ephemeral => ("Ephemeral", Color::Yellow),
    }
}
