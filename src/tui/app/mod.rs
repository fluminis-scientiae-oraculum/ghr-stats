//! The TUI's application state — what the dashboard knows, and what it can be
//! asked.
//!
//! This file owns the vocabulary ([`Tab`], [`LiveRunner`], [`Hits`], [`Overlay`])
//! and the [`App`] struct itself, plus the read-only accessors the view layer
//! calls. It deliberately holds no flow. The three things that MOVE state live in
//! children, one per direction, because they have never changed together:
//!
//! - [`sample`] — the world INTO [`App`]: the live in-memory fleet probe and the
//!   history read, once per tick.
//! - [`nav`] — the operator INTO [`App`]: keys and mouse, resolved to tab,
//!   selection and drill state.
//! - [`mutate`] — [`App`] back OUT to the world: the modal overlays, and the
//!   config writes they and the Config tab produce.
//!
//! Their import lists say the same thing. `collectors`/`now_epoch` appear only in
//! [`sample`], `MouseEvent`/`Rect` only in [`nav`], `wizard`/`MutateOutcome` only
//! in [`mutate`], and the model types appear here because a struct necessarily
//! names the types its filler produces — that overlap IS the interface.
//!
//! Each child carries its own `impl App`, which needs no visibility widening: a
//! child module can read its ancestor's private items, so [`App`]'s private
//! fields stay private to `tui::app` as a whole.
//!
//! The now-view (Summary + Detail live stats) is sampled LIVE in-memory each
//! tick (`collectors::collect_local`, display-only, never written) — so the
//! dashboard shows the fleet standalone in either mode. History (Trends, Detail
//! sparklines, Jobs, GitHub) comes from the [`DataSource`]:
//!
//! - **Ephemeral** (no collector): from a bounded in-memory ring the live sample
//!   fills each tick. Trends + sparklines show a since-launch window; Jobs +
//!   GitHub are empty (collector-only features).
//! - **Persistent** (collector reachable): from the collector over the IPC
//!   socket; the rings still fill as a warm fallback.
//!
//! The TUI never opens the database — the collector is the sole reader/writer of
//! it, and cross-scope access goes through the socket, not the file.

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
use crate::shared::hooks::install::HookStatus;
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::models::{BusyPoint, GhView, HistPoint, HostPoint, JobRow, Liveness, Mode};
use crate::shared::paths::Scope;
use crate::tui::history::{DataSource, Rings};
use crate::tui::input::action::{ActionKind, RecycleRunner, RestartRunner};
use crate::tui::widgets::wizard::WizardMode;

mod mutate;
mod nav;
mod sample;

const HISTORY_POINTS: usize = 120;
const TREND_POINTS: usize = 240;
const JOB_ROWS: usize = 200;

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
    pub gh: GhView,
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
    /// The Summary table's data-row region (below the header), for click-to-
    /// select. `None` on non-Summary views / when nothing is drawn there.
    pub table_rows: Option<Rect>,
    /// `(key, x_start, x_end_exclusive)` for each clickable footer hint, and the
    /// footer's row — a click on a hint is dispatched like pressing that key.
    pub footer: Vec<(KeyCode, u16, u16)>,
    pub footer_row: u16,
}

/// A modal popup drawn over the dashboard; while one is open the loop routes
/// every key to it. One concept for the three no-teardown modals (the config
/// wizard, the help sheet, and an informational block — e.g. privilege
/// guidance). Distinct from the suspend/resume path used for privileged
/// shell-outs (Restart/Recycle/hook-install), which tears the terminal down.
pub(crate) enum Overlay {
    Wizard(WizardMode),
    Help,
    /// Read-only guidance (title, body lines) — e.g. "this needs root, here's how".
    Info {
        title: String,
        body: String,
    },
}

pub(crate) struct App {
    cfg: Config,
    /// The `--config` override (if any), so the native config wizard writes back
    /// to the same file this run loaded; `None` ⇒ the scope's default path.
    config_path: Option<PathBuf>,
    /// The open modal popup, if any (config wizard / help / info block).
    overlay: Option<Overlay>,
    /// The history source: in-memory rings (Ephemeral) or the collector's IPC
    /// socket (Persistent). Re-probed each refresh.
    source: DataSource,
    /// Bounded in-memory history, filled from the live sample every tick — the
    /// Ephemeral-mode trends/sparklines, and the warm fallback in Persistent mode.
    rings: Rings,
    /// Derives per-runner CPU% from cgroup-usage deltas between ticks.
    cpu: CpuRateTracker,
    /// Per-runner liveness edge `(current, since_ts)`, tracked in-memory so
    /// "idle/active for <dur>" works standalone (no `serve`/DB needed). Keyed by
    /// install `dir` (locally unique) — agentId collides across orgs.
    edges: HashMap<String, (Liveness, i64)>,
    pub(crate) runners: Vec<LiveRunner>,
    pub(crate) host: Option<HostPoint>,
    pub(crate) tab: Tab,
    /// `Some(row)` when Summary is drilled into Detail for `runners[row]`.
    pub(crate) drill: Option<usize>,
    /// Selection + scroll offset for the Summary table. `RefCell` because the
    /// render pass (which only has `&App`) must write back ratatui's auto-scroll
    /// offset — otherwise `select_at_row` (mouse click) reads a stale offset and
    /// resolves the wrong runner once the list scrolls past one screen.
    pub(crate) table: RefCell<TableState>,
    pub(crate) detail_history: Vec<HistPoint>,
    pub(crate) detail_last_job: Option<JobRow>,
    pub(crate) trend_host: Vec<HostPoint>,
    pub(crate) trend_busy: Vec<BusyPoint>,
    pub(crate) jobs: Vec<JobRow>,
    pub(crate) api_state: HashMap<(String, i64), GhView>,
    /// Org logins with a configured read-only PAT. In Persistent mode this is the
    /// collector's authoritative view of the root-owned /etc config (presence
    /// only, no tokens); in Ephemeral mode it falls back to this run's loaded cfg.
    /// So the Config tab + GitHub-view messaging are correct even for a non-root
    /// TUI that cannot itself read /etc.
    configured_orgs: Vec<String>,
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
    pub(crate) hits: RefCell<Hits>,
    /// Last Summary row click `(row_index, when)` — for double-click → Detail.
    last_click: Option<(usize, Instant)>,
}

impl App {
    pub(crate) fn new(mut cfg: Config, config_path: Option<PathBuf>) -> Self {
        // With no configured roots (e.g. a non-root TUI that can't read the
        // root-owned /etc config), fall back to systemd-discovered roots so the
        // live view still finds the fleet. Resolved once — it shells out.
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

    /// The connected collector's build version — `None` in Ephemeral mode, or
    /// when talking to a collector too old to report one.
    pub(crate) fn collector_version(&self) -> Option<&str> {
        self.source.collector_version()
    }

    /// Why the dashboard is Ephemeral, when it is. Surfaced on the Config tab so
    /// "the service is an older build" is not silently indistinguishable from
    /// "there is no service".
    pub(crate) fn ephemeral_reason(&self) -> Option<&EphemeralReason> {
        self.source.ephemeral_reason()
    }

    /// The current data plane (Ephemeral / Persistent) — for the header badge
    /// and the Config tab.
    pub(crate) fn mode(&self) -> Mode {
        self.source.mode()
    }

    /// The scope of the connected collector, if any — the Config tab notes it
    /// when it differs from where this TUI would otherwise look.
    pub(crate) fn source_scope(&self) -> Option<Scope> {
        self.source.scope()
    }

    /// Whether any read-only PAT is configured (feeds GitHub-view messaging).
    /// Uses the collector-reported orgs (Persistent) / loaded cfg (Ephemeral) so a
    /// non-root TUI doesn't falsely report "no PAT" for a root-owned /etc config.
    pub(crate) fn has_tokens(&self) -> bool {
        !self.configured_orgs.is_empty()
    }

    /// Org logins with a configured read-only PAT (presence only) — for the
    /// Config tab's token list. See [`Self::configured_orgs`] field docs.
    pub(crate) fn configured_orgs(&self) -> &[String] {
        &self.configured_orgs
    }

    /// Whether the GitHub reconcile has returned any runner state this session.
    pub(crate) fn reconcile_populated(&self) -> bool {
        !self.api_state.is_empty()
    }

    /// How many runners carry the ghr-stats job hook (feeds Jobs-tab messaging).
    pub(crate) fn hooked_runner_count(&self) -> usize {
        self.runners
            .iter()
            .filter(|r| matches!(r.hook, HookStatus::Ours))
            .count()
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
}
