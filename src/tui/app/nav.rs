//! Keys and mouse resolved to tab, selection and drill state only. Clicks that
//! stand for a key return its [`KeyCode`] so the loop's `route_key` dispatches both.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::{App, Tab};

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

impl App {
    pub(crate) fn on_key(&mut self, code: KeyCode) {
        if code == KeyCode::Char('?') {
            self.open_help();
            return;
        }
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

    /// `Some(key)`: dispatch as if that key were pressed.
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
                // Snapshot so the `hits` borrow ends before `&mut self` calls.
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

    /// `Some(Enter)` on a second click on the same row within [`DOUBLE_CLICK`].
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
        if let Some(r) = sel.and_then(|i| self.runners.get(i)) {
            self.drill = Some(r.dir.clone());
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
