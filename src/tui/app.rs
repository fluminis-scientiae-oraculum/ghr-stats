//! TUI application state.
//!
//! The now-view (Summary + Detail live stats) is sampled LIVE in-memory each
//! tick (`collectors::collect_local`, display-only, never written) — so the
//! dashboard shows the fleet standalone in either mode. History (Trends, Detail
//! sparklines, Jobs, GitHub) comes from the [`history::DataSource`]:
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

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use crate::shared::collectors::cpu::CpuRateTracker;
use crate::shared::collectors::{self, runners};
use crate::shared::config::Config;
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::models::{
    ApiState, BusyPoint, HistPoint, HostPoint, JobRow, Liveness, RunnerState,
};
use crate::shared::paths::Scope;
use crate::shared::util::now_epoch;
use crate::tui::history::{DataSource, Mode, MutateOutcome, Rings};
use crate::tui::input::action::{ActionKind, RecycleRunner, RestartRunner};
use crate::tui::widgets::wizard::{self, WizardMode};

const HISTORY_POINTS: usize = 120;
const TREND_POINTS: usize = 240;
const JOB_ROWS: usize = 200;

/// Shown when the collector refuses a config mutation — the TUI's peer is neither
/// root nor in the `ghr-stats` group. The collector resolves group membership
/// fresh from the group DB by uid, so `usermod -aG` takes effect immediately (no
/// re-login needed); running the TUI as root also works.
const NOT_AUTHORIZED: &str = "not authorized — add yourself to the `ghr-stats` group \
    (`sudo usermod -aG ghr-stats $USER`) or run `sudo ghr-stats`";

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
    /// The Summary table's data-row region (below the header), for click-to-
    /// select. `None` on non-Summary views / when nothing is drawn there.
    pub table_rows: Option<Rect>,
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
    /// "idle/active for <dur>" works standalone (no `serve`/DB needed).
    edges: HashMap<i64, (Liveness, i64)>,
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
    pub(crate) status: Option<String>,
    pub(crate) should_quit: bool,
    pub(crate) hits: RefCell<Hits>,
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
            table,
            detail_history: Vec::new(),
            detail_active_job: None,
            trend_host: Vec::new(),
            trend_busy: Vec::new(),
            jobs: Vec::new(),
            api_state: HashMap::new(),
            status: None,
            should_quit: false,
            hits: RefCell::new(Hits::default()),
        }
    }

    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
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
    pub(crate) fn has_tokens(&self) -> bool {
        !self.cfg.github.tokens.is_empty()
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

    // ---- modal overlays (config wizard / help / info) ----

    /// Whether a modal overlay is open (⇒ the loop routes every key to it).
    pub(crate) fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    /// The open overlay to render, if any.
    pub(crate) fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    /// Open the config wizard at its action menu (from the Config tab `[a]`).
    pub(crate) fn open_wizard(&mut self) {
        self.overlay = Some(Overlay::Wizard(WizardMode::new()));
    }

    /// Open the context-sensitive help sheet (`[?]`).
    pub(crate) fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    /// Open a read-only info block (e.g. privilege guidance) — dismissed by any key.
    pub(crate) fn open_info(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.overlay = Some(Overlay::Info {
            title: title.into(),
            body: body.into(),
        });
    }

    /// Route one key to the open overlay. The wizard advances/closes via its
    /// typestate (and delegates text editing to its input widget, hence the full
    /// `KeyEvent`); help/info are dismissed by any key. A wizard close that
    /// changed the config reloads it so the Config tab reflects the new token.
    pub(crate) fn overlay_key(&mut self, key: KeyEvent) {
        match self.overlay.take() {
            Some(Overlay::Wizard(mode)) => {
                let ctx = self.wizard_ctx();
                let target = self.config_target();
                // Persist sink for the validated token: route through the root
                // collector (IPC) when Persistent+authorized, else fall back to a
                // direct config write (succeeds as root, else the "re-run with
                // sudo" error). Borrows only `self.source`, so it is released
                // before the `Close` arm reloads config.
                let source = &mut self.source;
                let save = |org: &str, token: &str| -> Result<(), String> {
                    match source.add_org_token(org, token) {
                        MutateOutcome::Mutated => Ok(()),
                        MutateOutcome::Denied => Err(NOT_AUTHORIZED.to_string()),
                        MutateOutcome::Unreachable => {
                            crate::shared::config::persist::set_org_token(&target, org, token)
                                .map_err(|e| e.to_string())
                        }
                    }
                };
                match mode.on_key(key, &ctx, save) {
                    wizard::Step::Stay(next) => self.overlay = Some(Overlay::Wizard(next)),
                    wizard::Step::Close(changed) => {
                        // Re-read to reflect the new token in the Config tab. As a
                        // non-root TUI that saved via IPC we can't read root-owned
                        // /etc — the reload is then a graceful no-op and the
                        // wizard's own "✓ saved" screen is the confirmation.
                        if changed {
                            self.reload_cfg();
                        }
                    }
                }
            }
            // Help / Info: any key dismisses (already removed by `take`).
            Some(Overlay::Help | Overlay::Info { .. }) | None => {}
        }
    }

    /// What the wizard needs to act: the locally-discovered runner ids (for the
    /// agentId match). The persist mechanism is injected separately (see
    /// [`Self::overlay_key`]), so `WizardCtx` no longer carries a write target.
    fn wizard_ctx(&self) -> wizard::WizardCtx {
        wizard::WizardCtx {
            local_ids: self.runners.iter().map(|r| r.agent_id).collect(),
        }
    }

    /// The config file the TUI writes — the `--config` override, else the
    /// canonical system config at `/etc` (matches `ghr-stats config`). Writing it
    /// needs root, so config edits work when the dashboard is run as `sudo
    /// ghr-stats`; a non-root edit fails with a clear "re-run with sudo" error.
    pub(crate) fn config_target(&self) -> PathBuf {
        crate::shared::paths::config_write_target(self.config_path.as_deref())
    }

    /// Toggle the Prometheus `/metrics` pull endpoint (Config `[m]`), persisting
    /// to `/etc`. Routes through the root collector (IPC) when Persistent+
    /// authorized, else a direct config write. Takes effect on the next `serve`
    /// start.
    pub(crate) fn toggle_metrics(&mut self) {
        let enabled = !self.cfg.metrics.pull.enabled;
        let addr = self.cfg.metrics.pull.addr.clone();
        let result = match self.source.set_metrics_pull(enabled, &addr) {
            MutateOutcome::Mutated => Ok(()),
            MutateOutcome::Denied => Err(NOT_AUTHORIZED.to_string()),
            MutateOutcome::Unreachable => {
                crate::shared::config::persist::set_metrics_pull(
                    &self.config_target(),
                    enabled,
                    &addr,
                )
                .map_err(|e| e.to_string())
            }
        };
        match result {
            Ok(()) => {
                // Mirror the one field locally so the Config tab flips immediately.
                // A non-root TUI that saved via IPC cannot re-read root-owned /etc,
                // so `reload_cfg` would be a no-op here — the collector already
                // persisted the change; this keeps the on-screen state truthful.
                self.cfg.metrics.pull.enabled = enabled;
                let state = if enabled { "enabled" } else { "disabled" };
                self.status = Some(format!(
                    "metrics pull {state} — restart the service to apply"
                ));
            }
            Err(e) => self.status = Some(format!("✗ metrics toggle failed: {e}")),
        }
    }

    /// Reload config from disk after a write, then refresh the views.
    fn reload_cfg(&mut self) {
        if let Ok(mut cfg) = Config::load(self.config_path.as_deref()) {
            cfg.runner_roots = runners::effective_roots(&cfg.runner_roots);
            self.cfg = cfg;
        }
        self.refresh();
    }

    /// Sample the fleet LIVE (in-memory, display-only) for the now-view, and read
    /// the DB for history + the GitHub view. Never writes — the single-writer
    /// invariant is `serve`'s.
    pub(crate) fn refresh(&mut self) {
        let now = now_epoch();
        // Live now-view: probe runners + host in-memory, like `serve`'s sampler
        // but without persisting. `walk_work=false` keeps it cheap (the _work
        // total is a slow trend metric, read from history instead).
        let snap = collectors::collect_local(&self.cfg.runner_roots, now, false);
        let sampled_at = Instant::now();
        let h = snap.host;
        let host = HostPoint {
            ts: h.ts,
            load1: h.load1,
            mem_used: h.mem_used,
            mem_total: h.mem_total,
            tmp_bytes: h.tmp_bytes,
            work_bytes: h.work_bytes,
            root_free: h.root_free,
        };
        self.rings.push_host(host.clone());
        self.host = Some(host);

        // Attach to a collector that started while we're open (no-op if already
        // Persistent; a later failed request reverts us to Ephemeral).
        self.source.reconnect_if_ephemeral();
        // GitHub's view is Persistent-only (from the collector's reconcile).
        let api = self.source.latest_api_runners();
        // The collector's persisted liveness edges (Persistent only) — the true,
        // restart-surviving `since_ts` for the "For" duration. Empty in Ephemeral.
        let persisted = self.source.runner_states();
        // A hook counts as "ours" if it points under ANY scope's hooks dir —
        // hooks always install System-scope (they need root), but this dashboard
        // is normally run non-root, so keying off `Scope::detect()` alone
        // mislabeled installed/chained hooks as foreign (cross-scope status bug).
        let our_dirs = [
            install::hooks_dir(&Scope::System.data_dir()),
            install::hooks_dir(&Scope::User.data_dir()),
        ];

        let mut edges = HashMap::with_capacity(snap.runners.len());
        let mut runners = Vec::with_capacity(snap.runners.len());
        let (mut busy, mut online) = (0u32, 0u32);
        for p in snap.runners {
            let id = p.info.agent_id;
            let cpu_pct = self.cpu.rate(id, p.cpu_usage_usec, sampled_at);
            // Feed the Ephemeral-mode sparkline ring from the same live sample.
            self.rings.push_runner(
                id,
                HistPoint {
                    ts: now,
                    cpu_pct,
                    mem_bytes: p.mem_bytes,
                },
            );
            match p.liveness {
                Liveness::Busy => {
                    busy += 1;
                    online += 1;
                }
                Liveness::Idle => online += 1,
                Liveness::Offline => {}
            }
            // In-memory liveness edge: keep `since` while unchanged, else now.
            let edge_since = match self.edges.get(&id) {
                Some((prev, since)) if *prev == p.liveness => *since,
                _ => now,
            };
            edges.insert(id, (p.liveness, edge_since));
            // Prefer the collector's persisted edge (survives TUI restarts) when it
            // agrees with the live-sampled liveness; else the in-memory edge
            // (Ephemeral, or a transition the collector hasn't persisted yet).
            let since = pick_since(persisted.get(&id), p.liveness, edge_since);
            runners.push(LiveRunner {
                liveness: p.liveness,
                cpu_pct,
                mem_bytes: p.mem_bytes,
                uptime_s: p.uptime_s,
                gh: api.get(&id).copied(),
                state_seconds: Some((now - since).max(0)),
                hook: install::detect_in(&p.info.dir, &our_dirs),
                work_folder: p.info.work_folder,
                agent_id: id,
                name: p.info.name,
                org: p.info.org,
                group: p.info.group,
                dir: p.info.dir,
                user: p.info.user,
            });
        }
        // Fleet occupancy for the Ephemeral busy-trend (reproduces busy_series).
        self.rings.push_busy(BusyPoint {
            ts: now,
            busy,
            online,
        });
        self.edges = edges;
        self.runners = runners;
        self.api_state = api;
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

    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown if self.scrollable() => self.move_selection(1),
            MouseEventKind::ScrollUp if self.scrollable() => self.move_selection(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                // A click resolves to at most one target; snapshot the hit cache,
                // then act (so the `hits` borrow is released before `&mut self`).
                let (tab, rows) = {
                    let hit = self.hits.borrow();
                    let tab = (m.row == hit.tab_row)
                        .then(|| {
                            hit.tabs
                                .iter()
                                .find(|(_, a, b)| m.column >= *a && m.column < *b)
                                .map(|(t, _, _)| *t)
                        })
                        .flatten();
                    (tab, hit.table_rows)
                };
                if let Some(t) = tab {
                    self.set_tab(t);
                } else if let Some(r) = rows {
                    self.select_at_row(r, m.column, m.row);
                }
            }
            _ => {}
        }
    }

    /// Select the Summary row under a click at `(col, row)`, if it lands on the
    /// table's data region and a runner exists there (respecting the scroll
    /// offset). Summary-only, like the scroll wheel.
    fn select_at_row(&mut self, region: Rect, col: u16, row: u16) {
        if !self.scrollable() {
            return;
        }
        let in_x = col >= region.x && col < region.x.saturating_add(region.width);
        let in_y = row >= region.y && row < region.y.saturating_add(region.height);
        if !in_x || !in_y {
            return;
        }
        let idx = self.table.offset() + (row - region.y) as usize;
        if idx < self.runners.len() {
            self.table.select(Some(idx));
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
        self.detail_history = self.source.runner_history(&self.rings, id, HISTORY_POINTS);
        self.detail_active_job = self.source.active_job(&name);
    }

    fn load_trends(&mut self) {
        self.trend_host = self.source.host_series(&self.rings, TREND_POINTS);
        self.trend_busy = self.source.busy_series(&self.rings, TREND_POINTS);
    }

    fn load_jobs(&mut self) {
        self.jobs = self.source.recent_jobs(JOB_ROWS);
    }
}

/// Choose the "since" timestamp for a runner's current-liveness duration (the
/// "For" column): the collector's persisted edge when it agrees with the live
/// liveness (true, restart-surviving), else the TUI's in-memory edge (Ephemeral,
/// or a transition the collector hasn't persisted yet). Pure + tested.
fn pick_since(persisted: Option<&RunnerState>, live: Liveness, edge_since: i64) -> i64 {
    match persisted {
        Some(st) if st.liveness == live => st.since_ts,
        _ => edge_since,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(liveness: Liveness, since_ts: i64) -> RunnerState {
        RunnerState {
            agent_id: 1,
            liveness,
            since_ts,
            last_seen_ts: since_ts,
        }
    }

    #[test]
    fn pick_since_prefers_persisted_edge_when_liveness_agrees() {
        let persisted = state(Liveness::Busy, 100);
        // Persisted agrees with live ⇒ use the persisted (restart-surviving) edge.
        assert_eq!(pick_since(Some(&persisted), Liveness::Busy, 900), 100);
    }

    #[test]
    fn pick_since_falls_back_on_disagreement_or_absence() {
        let persisted = state(Liveness::Idle, 100);
        // Live liveness changed since the collector's last write ⇒ in-memory edge.
        assert_eq!(pick_since(Some(&persisted), Liveness::Busy, 900), 900);
        // No persisted edge (Ephemeral) ⇒ in-memory edge.
        assert_eq!(pick_since(None, Liveness::Busy, 900), 900);
    }
}
