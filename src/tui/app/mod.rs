//! TUI state and its read-only accessors. [`sample`] moves the world into [`App`],
//! [`nav`] the operator's input, [`mutate`] App's changes back out.
//!
//! Live stats are sampled in-memory each tick; history comes from [`DataSource`].
//! The TUI never opens the database: the collector is its sole reader/writer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use crate::shared::collectors::cpu::CpuRateTracker;
use crate::shared::collectors::runners;
use crate::shared::config::Config;
use crate::shared::github::RunnerScope;
use crate::shared::hooks::install::HookStatus;
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::models::{BusyPoint, GhView, HistPoint, HostPoint, JobRow, Liveness, Mode};
use crate::shared::paths::Scope;
use crate::tui::input::action::{ActionKind, RecycleRunner, RestartRunner};
use crate::tui::overlay::wizard::WizardMode;
use crate::tui::source::{DataSource, Rings};

mod mutate;
mod nav;
mod sample;

const HISTORY_POINTS: usize = 120;
const TREND_POINTS: usize = 240;
const JOB_ROWS: usize = 200;

/// `Detail` is a drill-down from `Summary`, not a tab.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Summary,
    Jobs,
    Trends,
    Config,
    Quit,
}

impl Tab {
    pub(crate) const BAR: [Tab; 5] = [Tab::Summary, Tab::Jobs, Tab::Trends, Tab::Config, Tab::Quit];
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

pub(crate) struct LiveRunner {
    pub agent_id: i64,
    pub name: String,
    pub org: String,
    pub scope: RunnerScope,
    pub group: Option<String>,
    pub dir: PathBuf,
    pub user: String,
    pub liveness: Liveness,
    pub cpu_pct: Option<f32>,
    pub mem_bytes: Option<u64>,
    pub uptime_s: Option<u64>,
    pub gh: GhView,
    pub work_folder: String,
    /// Seconds in the current liveness state.
    pub state_seconds: Option<i64>,
    pub hook: HookStatus,
}

/// Click targets cached during render (ratatui is immediate-mode) for the mouse handler.
#[derive(Default)]
pub(crate) struct Hits {
    /// `(tab, x_start, x_end_exclusive)` on the tab-bar row.
    pub tabs: Vec<(Tab, u16, u16)>,
    pub tab_row: u16,
    /// Summary table's data rows, below the header; `None` when not drawn.
    pub table_rows: Option<Rect>,
    /// `(key, x_start, x_end_exclusive)` per clickable footer hint.
    pub footer: Vec<(KeyCode, u16, u16)>,
    pub footer_row: u16,
}

/// Modal popup that takes every key while open; never tears down the terminal.
pub(crate) enum Overlay {
    Wizard(WizardMode),
    Help,
    Info { title: String, body: String },
}

pub(crate) struct App {
    cfg: Config,
    /// `--config` override; `None` ⇒ the scope's default path.
    config_path: Option<PathBuf>,
    overlay: Option<Overlay>,
    /// Re-probed each refresh.
    source: DataSource,
    /// Ephemeral-mode history; fallback in Persistent mode.
    rings: Rings,
    cpu: CpuRateTracker,
    /// Liveness edge `(current, since_ts)`, keyed by install dir: agentId collides across orgs.
    edges: HashMap<String, (Liveness, i64)>,
    pub(crate) runners: Vec<LiveRunner>,
    pub(crate) host: Option<HostPoint>,
    pub(crate) tab: Tab,
    /// `Some(row)` when Summary is drilled into Detail for `runners[row]`.
    pub(crate) drill: Option<usize>,
    /// Render writes back ratatui's auto-scroll offset; `select_or_open` needs it fresh.
    pub(crate) table: RefCell<TableState>,
    pub(crate) detail_history: Vec<HistPoint>,
    pub(crate) detail_last_job: Option<JobRow>,
    pub(crate) trend_host: Vec<HostPoint>,
    pub(crate) trend_busy: Vec<BusyPoint>,
    pub(crate) jobs: Vec<JobRow>,
    pub(crate) api_state: HashMap<(String, i64), GhView>,
    /// Orgs with a PAT: collector-reported in Persistent mode, since a non-root TUI
    /// cannot read /etc; this run's cfg in Ephemeral mode.
    configured_orgs: Vec<String>,
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
    pub(crate) hits: RefCell<Hits>,
    /// For double-click → Detail.
    last_click: Option<(usize, Instant)>,
}

impl App {
    pub(crate) fn new(mut cfg: Config, config_path: Option<PathBuf>) -> Self {
        // Resolved once: it shells out.
        cfg.runner_roots = runners::effective_roots(&cfg.runner_roots);
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            cfg,
            config_path,
            overlay: None,
            source: DataSource::detect(),
            rings: Rings::new(TREND_POINTS, HISTORY_POINTS),
            cpu: CpuRateTracker::new(),
            edges: HashMap::new(),
            runners: Vec::new(),
            host: None,
            tab: Tab::Summary,
            drill: None,
            table: RefCell::new(table),
            detail_history: Vec::new(),
            detail_last_job: None,
            trend_host: Vec::new(),
            trend_busy: Vec::new(),
            jobs: Vec::new(),
            api_state: HashMap::new(),
            configured_orgs: Vec::new(),
            status: None,
            should_quit: false,
            hits: RefCell::new(Hits::default()),
            last_click: None,
        }
    }

    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// `None` in Ephemeral mode or from a collector too old to report one.
    pub(crate) fn collector_version(&self) -> Option<&str> {
        self.source.collector_version()
    }

    pub(crate) fn ephemeral_reason(&self) -> Option<&EphemeralReason> {
        self.source.ephemeral_reason()
    }

    pub(crate) fn mode(&self) -> Mode {
        self.source.mode()
    }

    pub(crate) fn source_scope(&self) -> Option<Scope> {
        self.source.scope()
    }

    pub(crate) fn has_tokens(&self) -> bool {
        !self.configured_orgs.is_empty()
    }

    pub(crate) fn configured_orgs(&self) -> &[String] {
        &self.configured_orgs
    }

    /// The GitHub reconcile has returned runner state this session.
    pub(crate) fn reconcile_populated(&self) -> bool {
        !self.api_state.is_empty()
    }

    pub(crate) fn hooked_runner_count(&self) -> usize {
        self.runners
            .iter()
            .filter(|r| matches!(r.hook, HookStatus::Ours))
            .count()
    }

    pub(crate) fn detail_runner(&self) -> Option<&LiveRunner> {
        self.drill.and_then(|i| self.runners.get(i))
    }

    pub(crate) fn restart_action(&self) -> Result<ActionKind, String> {
        let r = self.detail_runner().ok_or("no runner selected")?;
        let unit = runners::unit_for(&r.dir)?;
        Ok(ActionKind::Restart(RestartRunner {
            unit,
            agent_id: r.agent_id,
            busy: r.liveness == Liveness::Busy,
        }))
    }

    pub(crate) fn recycle_action(&self) -> Result<ActionKind, String> {
        let r = self.detail_runner().ok_or("no runner selected")?;
        if r.liveness != Liveness::Idle {
            return Err(format!(
                "{} is not idle; recycle waits for an idle runner",
                r.name
            ));
        }
        let unit = runners::unit_for(&r.dir)?;
        Ok(ActionKind::Recycle(RecycleRunner {
            unit,
            agent_id: r.agent_id,
            install_dir: r.dir.clone(),
            work_folder: r.work_folder.clone(),
        }))
    }
}
