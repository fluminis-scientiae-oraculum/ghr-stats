//! Driving [`App`] from the operator: keys and mouse resolved to navigation.
//!
//! Every handler here ends in tab, selection or drill state and nothing else —
//! no config write, no collector call. That is why a footer-hint click returns a
//! [`KeyCode`] rather than acting: a click and the key it depicts must take the
//! SAME path, so the two can never drift, and the loop's `route_key` stays the
//! one place an action is dispatched.
//!
//! Mouse handling reads the hit cache [`super::Hits`] that the render pass
//! populates — ratatui is immediate-mode, so the geometry a click lands on is
//! only known from the frame that drew it.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::{App, Tab};

/// Max gap between two clicks on the same Summary row to count as a double-click
/// (opens Detail). Chosen to match typical desktop double-click timing.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

impl App {
    pub(crate) fn on_key(&mut self, code: KeyCode) {
        // Help is global — it opens over any view/mode.
        if code == KeyCode::Char('?') {
            self.open_help();
            return;
        }
        // While drilled into Detail, keys are back-nav / refresh only.
        if self.drill.is_some() {
            match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
                    self.drill = None;
                }
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => self.cycle_tab(1),
            KeyCode::BackTab => self.cycle_tab(-1),
            KeyCode::Char('1') => self.set_tab(Tab::Summary),
            KeyCode::Char('2') => self.set_tab(Tab::Jobs),
            KeyCode::Char('3') => self.set_tab(Tab::Trends),
            KeyCode::Char('4') => self.set_tab(Tab::Config),
            KeyCode::Char('r') => self.refresh(),
            _ if self.tab == Tab::Summary => match code {
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.enter_detail(),
                _ => {}
            },
            _ => {}
        }
    }

    /// Handle a mouse event. Returns `Some(key)` when a click should be
    /// dispatched as if that key were pressed — a footer-hint click maps to its
    /// key, and a double-click on a runner row maps to `Enter` (open Detail) —
    /// so `route_mouse` can reuse the full keyboard action path. `None` when the
    /// event was fully handled here (scroll, tab click, single-click select).
    pub(crate) fn on_mouse(&mut self, m: MouseEvent) -> Option<KeyCode> {
        match m.kind {
            MouseEventKind::ScrollDown if self.scrollable() => {
                self.move_selection(1);
                None
            }
            MouseEventKind::ScrollUp if self.scrollable() => {
                self.move_selection(-1);
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A click resolves to at most one target; snapshot the hit cache,
                // then act (so the `hits` borrow is released before `&mut self`).
                let (tab, footer_key, rows) = {
                    let hit = self.hits.borrow();
                    let tab = (m.row == hit.tab_row)
                        .then(|| {
                            hit.tabs
                                .iter()
                                .find(|(_, a, b)| m.column >= *a && m.column < *b)
                                .map(|(t, _, _)| *t)
                        })
                        .flatten();
                    let footer_key = (m.row == hit.footer_row)
                        .then(|| {
                            hit.footer
                                .iter()
                                .find(|(_, a, b)| m.column >= *a && m.column < *b)
                                .map(|(k, _, _)| *k)
                        })
                        .flatten();
                    (tab, footer_key, hit.table_rows)
                };
                // A footer hint acts like pressing its key (routed via route_key).
                if let Some(k) = footer_key {
                    return Some(k);
                }
                if let Some(t) = tab {
                    self.set_tab(t);
                    return None;
                }
                if let Some(r) = rows {
                    return self.select_or_open(r, m.column, m.row);
                }
                None
            }
            _ => None,
        }
    }

    /// Select the Summary row under a click at `(col, row)`, if it lands on the
    /// table's data region and a runner exists there (respecting the scroll
    /// offset). Summary-only, like the scroll wheel.
    /// Select the runner row under a click, and return `Some(Enter)` when it is
    /// the second click on the same row within [`DOUBLE_CLICK`] — a double-click
    /// opens Detail (dispatched as `Enter` via `route_key`). Summary-only.
    fn select_or_open(&mut self, region: Rect, col: u16, row: u16) -> Option<KeyCode> {
        if !self.scrollable() {
            return None;
        }
        let in_x = col >= region.x && col < region.x.saturating_add(region.width);
        let in_y = row >= region.y && row < region.y.saturating_add(region.height);
        if !in_x || !in_y {
            return None;
        }
        let idx = self.table.borrow().offset() + (row - region.y) as usize;
        if idx >= self.runners.len() {
            return None;
        }
        self.table.borrow_mut().select(Some(idx));
        let now = Instant::now();
        let double = matches!(self.last_click, Some((prev, t)) if prev == idx && now.duration_since(t) <= DOUBLE_CLICK);
        // Reset after a double (so a third click starts fresh); else record this.
        self.last_click = if double { None } else { Some((idx, now)) };
        double.then_some(KeyCode::Enter)
    }

    fn scrollable(&self) -> bool {
        self.drill.is_none() && self.tab == Tab::Summary
    }

    fn set_tab(&mut self, t: Tab) {
        if t == Tab::Quit {
            self.should_quit = true;
            return;
        }
        self.tab = t;
        self.drill = None;
        match t {
            Tab::Trends => self.load_trends(),
            Tab::Jobs => self.load_jobs(),
            _ => {}
        }
    }

    fn cycle_tab(&mut self, delta: i64) {
        let i = Tab::VIEWS.iter().position(|t| *t == self.tab).unwrap_or(0) as i64;
        let n = Tab::VIEWS.len() as i64;
        self.set_tab(Tab::VIEWS[(i + delta).rem_euclid(n) as usize]);
    }

    fn enter_detail(&mut self) {
        let sel = self.table.borrow().selected(); // release the borrow before load_detail
        if let Some(i) = sel
            && i < self.runners.len()
        {
            self.drill = Some(i);
            self.load_detail();
        }
    }

    fn move_selection(&mut self, delta: i64) {
        if self.runners.is_empty() {
            return;
        }
        let len = self.runners.len() as i64;
        let cur = self.table.borrow().selected().unwrap_or(0) as i64;
        self.table
            .borrow_mut()
            .select(Some((cur + delta).rem_euclid(len) as usize));
    }

    pub(super) fn clamp_selection(&mut self) {
        if self.runners.is_empty() {
            self.table.borrow_mut().select(None);
        } else {
            let i = self
                .table
                .borrow()
                .selected()
                .unwrap_or(0)
                .min(self.runners.len() - 1);
            self.table.borrow_mut().select(Some(i));
        }
    }
}
