//! TUI application state.
//!
//! The dashboard is a *reader*: for the live "now" view it re-runs the cheap
//! local collectors directly (so it works even when the collector daemon is
//! not running), and it reads SQLite for per-runner history (sparklines).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::crossterm::event::KeyCode;
use ratatui::widgets::TableState;

use crate::collectors::cpu::CpuRateTracker;
use crate::collectors::{self};
use crate::config::Config;
use crate::model::{HostSample, Liveness};
use crate::store::Store;
use crate::store::reader::{self, ApiState, BusyPoint, HistPoint, HostPoint, JobRow};
use crate::util::now_epoch;

/// How many historical points to pull for the detail sparklines.
const HISTORY_POINTS: usize = 120;
/// How many points to pull for the fleet trend charts.
const TREND_POINTS: usize = 240;
/// How many recent jobs to list.
const JOB_ROWS: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Overview,
    Detail,
    Trends,
    Jobs,
}

/// A runner as shown in the live view (probe + derived CPU%).
pub(crate) struct LiveRunner {
    pub agent_id: i64,
    pub name: String,
    pub org: String,
    pub group: Option<String>,
    pub dir: PathBuf,
    pub user: String,
    pub liveness: Liveness,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub uptime_s: Option<u64>,
    /// GitHub's view of this runner (from the latest API reconcile), if any.
    pub gh: Option<ApiState>,
}

pub(crate) struct App {
    cfg: Config,
    /// Read-only handle for history; `None` if the DB could not be opened.
    store: Option<Store>,
    cpu: CpuRateTracker,
    pub(crate) runners: Vec<LiveRunner>,
    pub(crate) host: Option<HostSample>,
    pub(crate) view: View,
    pub(crate) table: TableState,
    pub(crate) detail_history: Vec<HistPoint>,
    pub(crate) trend_host: Vec<HostPoint>,
    pub(crate) trend_busy: Vec<BusyPoint>,
    pub(crate) jobs: Vec<JobRow>,
    /// GitHub's latest view of every known runner, keyed by agent_id.
    pub(crate) api_state: HashMap<i64, ApiState>,
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
}

impl App {
    pub(crate) fn new(cfg: Config) -> Self {
        let (store, status) = match Store::open(&cfg.db_path) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("history unavailable: {e}"))),
        };
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            cfg,
            store,
            cpu: CpuRateTracker::new(),
            runners: Vec::new(),
            host: None,
            view: View::Overview,
            table,
            detail_history: Vec::new(),
            trend_host: Vec::new(),
            trend_busy: Vec::new(),
            jobs: Vec::new(),
            api_state: HashMap::new(),
            status,
            should_quit: false,
        }
    }

    /// Re-sample local sources for the live view (cheap: no `_work` walk).
    pub(crate) fn refresh(&mut self) {
        let now = now_epoch();
        let snap = collectors::collect_local(&self.cfg.runner_roots, now, false);
        let api = self
            .store
            .as_ref()
            .and_then(|s| reader::latest_api_runners(s.conn()).ok())
            .unwrap_or_default();
        let at = Instant::now();
        self.runners = snap
            .runners
            .into_iter()
            .map(|p| LiveRunner {
                agent_id: p.info.agent_id,
                cpu_pct: self.cpu.rate(p.info.agent_id, p.cpu_usage_usec, at),
                gh: api.get(&p.info.agent_id).copied(),
                name: p.info.name,
                org: p.info.org,
                group: p.info.group,
                dir: p.info.dir,
                user: p.info.user,
                liveness: p.liveness,
                mem_bytes: p.mem_bytes,
                uptime_s: p.uptime_s,
            })
            .collect();
        self.api_state = api;
        self.host = Some(snap.host);
        self.clamp_selection();
        match self.view {
            View::Detail => self.load_detail_history(),
            View::Trends => self.load_trends(),
            View::Jobs => self.load_jobs(),
            View::Overview => {}
        }
    }

    pub(crate) fn selected_runner(&self) -> Option<&LiveRunner> {
        self.table.selected().and_then(|i| self.runners.get(i))
    }

    pub(crate) fn on_key(&mut self, code: KeyCode) {
        match self.view {
            View::Overview => match code {
                KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
                KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.enter_detail(),
                KeyCode::Tab | KeyCode::Char('t') => self.enter_trends(),
                KeyCode::Char('J') | KeyCode::Char('w') => self.enter_jobs(),
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
            View::Detail => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
                    self.view = View::Overview;
                }
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
            View::Trends => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Esc | KeyCode::Tab | KeyCode::Char('t') => self.view = View::Overview,
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
            View::Jobs => match code {
                KeyCode::Char('q') => self.should_quit = true,
                KeyCode::Esc | KeyCode::Char('w') | KeyCode::Char('J') => {
                    self.view = View::Overview
                }
                KeyCode::Char('r') => self.refresh(),
                _ => {}
            },
        }
    }

    fn enter_detail(&mut self) {
        if self.selected_runner().is_some() {
            self.view = View::Detail;
            self.load_detail_history();
        }
    }

    fn enter_trends(&mut self) {
        self.view = View::Trends;
        self.load_trends();
    }

    fn enter_jobs(&mut self) {
        self.view = View::Jobs;
        self.load_jobs();
    }

    fn move_selection(&mut self, delta: i64) {
        if self.runners.is_empty() {
            return;
        }
        let len = self.runners.len() as i64;
        let cur = self.table.selected().unwrap_or(0) as i64;
        self.table
            .select(Some((cur + delta).rem_euclid(len) as usize));
    }

    fn clamp_selection(&mut self) {
        if self.runners.is_empty() {
            self.table.select(None);
        } else {
            let i = self
                .table
                .selected()
                .unwrap_or(0)
                .min(self.runners.len() - 1);
            self.table.select(Some(i));
        }
    }

    fn load_detail_history(&mut self) {
        let Some(id) = self.selected_runner().map(|r| r.agent_id) else {
            self.detail_history.clear();
            return;
        };
        if let Some(store) = &self.store {
            match reader::runner_history(store.conn(), id, HISTORY_POINTS) {
                Ok(h) => self.detail_history = h,
                Err(e) => self.status = Some(format!("history query failed: {e}")),
            }
        }
    }

    fn load_trends(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match reader::host_series(store.conn(), TREND_POINTS) {
            Ok(h) => self.trend_host = h,
            Err(e) => self.status = Some(format!("trend query failed: {e}")),
        }
        match reader::busy_series(store.conn(), TREND_POINTS) {
            Ok(b) => self.trend_busy = b,
            Err(e) => self.status = Some(format!("trend query failed: {e}")),
        }
    }

    fn load_jobs(&mut self) {
        if let Some(store) = &self.store {
            match reader::recent_jobs(store.conn(), JOB_ROWS) {
                Ok(j) => self.jobs = j,
                Err(e) => self.status = Some(format!("jobs query failed: {e}")),
            }
        }
    }
}
