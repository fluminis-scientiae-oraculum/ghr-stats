//! Filling [`App`] from the world, once per tick.
//!
//! Two sources, deliberately kept in one file because a tick reads both and
//! neither is meaningful alone: the LIVE in-memory fleet probe that drives the
//! now-view, and the [`super::DataSource`] read that backs history. Nothing here
//! writes to disk — the single-writer invariant belongs to `serve`.
//!
//! The one subtlety worth stating up front is the keying. All per-runner LOCAL
//! state is keyed by install `dir`, never by `agent_id`, because agentId is
//! unique only within an org and collides across the fleet; GitHub's view is
//! joined back by `(org, agent_id)`, where `org` disambiguates.

use std::collections::HashMap;
use std::time::Instant;

use crate::shared::collectors;
use crate::shared::hooks::install;
use crate::shared::models::{BusyPoint, GhView, HistPoint, HostPoint, Liveness, RunnerState};
use crate::shared::paths::Scope;
use crate::shared::util::now_epoch;

use super::{App, HISTORY_POINTS, JOB_ROWS, LiveRunner, TREND_POINTS, Tab};

impl App {
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
        // Configured token orgs: the collector's authoritative /etc view when
        // Persistent (so a non-root TUI reflects the real PATs), else this run's
        // own loaded config when Ephemeral.
        let orgs = self.source.configured_token_orgs();
        self.configured_orgs =
            orgs.unwrap_or_else(|| self.cfg.github.tokens.keys().cloned().collect());
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
            // Local identity is the install dir (locally unique), NOT agent_id —
            // agentId collides across orgs, which cross-contaminated CPU, the
            // liveness edge, and the sparkline ring. Key all per-runner local
            // state by `dirkey`; join GitHub's view by `(org, agent_id)`.
            let dirkey = p.info.dir.to_string_lossy().into_owned();
            let cpu_pct = self
                .cpu
                .rate(p.info.dir.clone(), p.cpu_usage_usec, sampled_at);
            // Feed the Ephemeral-mode sparkline ring from the same live sample.
            self.rings.push_runner(
                dirkey.clone(),
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
            let edge_since = match self.edges.get(&dirkey) {
                Some((prev, since)) if *prev == p.liveness => *since,
                _ => now,
            };
            edges.insert(dirkey.clone(), (p.liveness, edge_since));
            // Prefer the collector's persisted edge (survives TUI restarts) when it
            // agrees with the live-sampled liveness; else the in-memory edge
            // (Ephemeral, or a transition the collector hasn't persisted yet).
            let since = pick_since(persisted.get(&dirkey), p.liveness, edge_since);
            runners.push(LiveRunner {
                liveness: p.liveness,
                cpu_pct,
                mem_bytes: p.mem_bytes,
                uptime_s: p.uptime_s,
                gh: api
                    .get(&(p.info.org.clone(), id))
                    .copied()
                    .unwrap_or(GhView::Unknown),
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
            // Ephemeral mode has no collector and therefore no GitHub reconcile.
            // `None` plots a gap; emitting 0 would draw "GitHub says nothing is
            // online" for a fleet nobody has asked GitHub about.
            github: None,
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

    pub(super) fn load_detail(&mut self) {
        let Some((dir, name)) = self
            .detail_runner()
            .map(|r| (r.dir.to_string_lossy().into_owned(), r.name.clone()))
        else {
            self.detail_history.clear();
            self.detail_last_job = None;
            return;
        };
        self.detail_history = self
            .source
            .runner_history(&self.rings, &dir, HISTORY_POINTS);
        self.detail_last_job = self.source.latest_job(&name);
    }

    pub(super) fn load_trends(&mut self) {
        self.trend_host = self.source.host_series(&self.rings, TREND_POINTS);
        self.trend_busy = self.source.busy_series(&self.rings, TREND_POINTS);
    }

    pub(super) fn load_jobs(&mut self) {
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
            dir: "/srv/r1".into(),
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
