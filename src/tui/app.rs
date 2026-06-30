//! TUI application state — a PURE reader.
//!
//! `serve` is the sole sampler; the dashboard never samples. It reads the
//! latest fleet metrics from SQLite and joins them with each runner's static
//! identity from its `.runner` file (cheap, world-readable). If `serve` has not
//! written fresh samples, the Summary shows a banner instead of stale data.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::widgets::TableState;

use crate::collectors::runners;
use crate::config::Config;
use crate::hooks::install::{self, HookStatus};
use crate::model::Liveness;
use crate::paths::Scope;
use crate::store::Store;
use crate::store::reader::{self, ApiState, BusyPoint, HistPoint, HostPoint, JobRow};
use crate::tui::action::{ActionKind, RecycleRunner, RestartRunner};
use crate::util::now_epoch;

const HISTORY_POINTS: usize = 120;
const TREND_POINTS: usize = 240;
const JOB_ROWS: usize = 200;
/// Samples older than this many local intervals ⇒ "stale" (show the banner).
const STALE_TICKS: i64 = 4;

/// Top-level tabs. `Detail` is a drill-down from `Summary`, not a tab.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Summary,
    Jobs,
    Trends,
    Config,
    Quit,
}

impl Tab {
    /// Order shown in the tab bar.
    pub(crate) const BAR: [Tab; 5] = [Tab::Summary, Tab::Jobs, Tab::Trends, Tab::Config, Tab::Quit];
    /// The selectable views (Quit is an action, not a view).
    const VIEWS: [Tab; 4] = [Tab::Summary, Tab::Jobs, Tab::Trends, Tab::Config];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tab::Summary => "Summary",
            Tab::Jobs => "Jobs",
            Tab::Trends => "Trends",
            Tab::Config => "Config",
            Tab::Quit => "Quit",
        }
    }
}

/// A runner as shown in the live view: static identity + latest DB metrics.
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
    pub work_folder: String,
    /// Seconds in the current liveness state (`now - since_ts`), if known.
    pub state_seconds: Option<i64>,
    pub hook: HookStatus,
}

/// Click targets cached during render (ratatui is immediate-mode), read by the
/// mouse handler. Interior-mutable so the `&App` renderer can populate it.
#[derive(Default)]
pub(crate) struct Hits {
    /// `(tab, x_start, x_end_exclusive)` on the tab-bar row.
    pub tabs: Vec<(Tab, u16, u16)>,
    pub tab_row: u16,
}

pub(crate) struct App {
    cfg: Config,
    /// Read-only handle; `None` if the DB could not be opened.
    store: Option<Store>,
    pub(crate) runners: Vec<LiveRunner>,
    pub(crate) host: Option<HostPoint>,
    pub(crate) tab: Tab,
    /// `Some(row)` when Summary is drilled into Detail for `runners[row]`.
    pub(crate) drill: Option<usize>,
    pub(crate) table: TableState,
    pub(crate) detail_history: Vec<HistPoint>,
    pub(crate) detail_active_job: Option<JobRow>,
    pub(crate) trend_host: Vec<HostPoint>,
    pub(crate) trend_busy: Vec<BusyPoint>,
    pub(crate) jobs: Vec<JobRow>,
    pub(crate) api_state: HashMap<i64, ApiState>,
    /// Age (seconds) of the freshest runner sample; `None` if never sampled.
    pub(crate) sample_age: Option<i64>,
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
    pub(crate) hits: RefCell<Hits>,
}

impl App {
    pub(crate) fn new(cfg: Config) -> Self {
        let (store, status) = match Store::open(&cfg.db_path) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("database unavailable: {e}"))),
        };
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            cfg,
            store,
            runners: Vec::new(),
            host: None,
            tab: Tab::Summary,
            drill: None,
            table,
            detail_history: Vec::new(),
            detail_active_job: None,
            trend_host: Vec::new(),
            trend_busy: Vec::new(),
            jobs: Vec::new(),
            api_state: HashMap::new(),
            sample_age: None,
            status,
            should_quit: false,
            hits: RefCell::new(Hits::default()),
        }
    }

    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// Re-read the latest fleet state from the DB and re-join identity.
    pub(crate) fn refresh(&mut self) {
        let now = now_epoch();
        let infos = runners::discover(&self.cfg.runner_roots);
        let latest = self.read(reader::latest_runners).unwrap_or_default();
        let max_ts = latest.iter().map(|r| r.ts).max();
        let by_id: HashMap<i64, _> = latest.into_iter().map(|r| (r.agent_id, r)).collect();
        let api = self.read(reader::latest_api_runners).unwrap_or_default();
        let states = self.read(reader::runner_states).unwrap_or_default();
        let our_dir = install::hooks_dir(&Scope::detect().data_dir());
        self.host = self
            .store
            .as_ref()
            .and_then(|s| reader::latest_host(s.conn()).ok().flatten());

        self.runners = infos
            .into_iter()
            .map(|info| {
                let m = by_id.get(&info.agent_id);
                let state_seconds = states
                    .get(&info.agent_id)
                    .map(|st| (now - st.since_ts).max(0));
                let hook = install::detect(&info.dir, &our_dir);
                LiveRunner {
                    liveness: m.map(|s| s.liveness).unwrap_or(Liveness::Offline),
                    cpu_pct: m.and_then(|s| s.cpu_pct),
                    mem_bytes: m.and_then(|s| s.mem_bytes),
                    uptime_s: m.and_then(|s| s.uptime_s),
                    gh: api.get(&info.agent_id).copied(),
                    state_seconds,
                    hook,
                    work_folder: info.work_folder,
                    agent_id: info.agent_id,
                    name: info.name,
                    org: info.org,
                    group: info.group,
                    dir: info.dir,
                    user: info.user,
                }
            })
            .collect();
        self.api_state = api;
        self.sample_age = max_ts.map(|ts| (now - ts).max(0));
        self.clamp_selection();

        match self.tab {
            Tab::Trends => self.load_trends(),
            Tab::Jobs => self.load_jobs(),
            _ => {}
        }
        if self.drill.is_some() {
            self.load_detail();
        }
    }

    /// Run a reader query against the store, if open.
    fn read<T, F>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&rusqlite::Connection) -> crate::error::Result<T>,
    {
        self.store.as_ref().and_then(|s| f(s.conn()).ok())
    }

    /// True when there is no fresh sample (daemon down, or never run).
    pub(crate) fn is_stale(&self) -> bool {
        match self.sample_age {
            None => true,
            Some(age) => age > STALE_TICKS * self.cfg.intervals.local_secs.max(1) as i64,
        }
    }

    /// A banner to show when there is no live data, else `None`.
    pub(crate) fn banner(&self) -> Option<String> {
        if self.store.is_none() {
            return self.status.clone();
        }
        self.is_stale().then(|| {
            "no fresh data — start the sampler: `ghr-stats serve` (or install the service via Config)"
                .to_string()
        })
    }

    pub(crate) fn detail_runner(&self) -> Option<&LiveRunner> {
        self.drill.and_then(|i| self.runners.get(i))
    }

    /// Build a Restart action for the drilled runner (None if none is drilled or
    /// the runner has no `.service` unit file).
    pub(crate) fn restart_action(&self) -> Option<ActionKind> {
        let r = self.detail_runner()?;
        let unit = runners::unit_name(&r.dir)?;
        Some(ActionKind::Restart(RestartRunner {
            unit,
            agent_id: r.agent_id,
        }))
    }

    /// Build a Recycle action for the drilled runner — idle-only (None if it is
    /// busy/offline, none is drilled, or there is no unit file).
    pub(crate) fn recycle_action(&self) -> Option<ActionKind> {
        let r = self.detail_runner()?;
        if r.liveness != Liveness::Idle {
            return None;
        }
        let unit = runners::unit_name(&r.dir)?;
        Some(ActionKind::Recycle(RecycleRunner {
            unit,
            agent_id: r.agent_id,
            install_dir: r.dir.clone(),
            work_folder: r.work_folder.clone(),
        }))
    }

    pub(crate) fn on_key(&mut self, code: KeyCode) {
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

    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown if self.scrollable() => self.move_selection(1),
            MouseEventKind::ScrollUp if self.scrollable() => self.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                let clicked = {
                    let hit = self.hits.borrow();
                    if m.row == hit.tab_row {
                        hit.tabs
                            .iter()
                            .find(|(_, a, b)| m.column >= *a && m.column < *b)
                            .map(|(t, _, _)| *t)
                    } else {
                        None
                    }
                };
                if let Some(t) = clicked {
                    self.set_tab(t);
                }
            }
            _ => {}
        }
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
        if let Some(i) = self.table.selected()
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

    fn load_detail(&mut self) {
        let Some((id, name)) = self.detail_runner().map(|r| (r.agent_id, r.name.clone())) else {
            self.detail_history.clear();
            self.detail_active_job = None;
            return;
        };
        if let Some(h) = self.read(|c| reader::runner_history(c, id, HISTORY_POINTS)) {
            self.detail_history = h;
        }
        self.detail_active_job = self.read(|c| reader::active_job(c, &name)).flatten();
    }

    fn load_trends(&mut self) {
        if let Some(h) = self.read(|c| reader::host_series(c, TREND_POINTS)) {
            self.trend_host = h;
        }
        if let Some(b) = self.read(|c| reader::busy_series(c, TREND_POINTS)) {
            self.trend_busy = b;
        }
    }

    fn load_jobs(&mut self) {
        if let Some(j) = self.read(|c| reader::recent_jobs(c, JOB_ROWS)) {
            self.jobs = j;
        }
    }
}
