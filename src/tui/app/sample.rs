//! Fills [`App`] each tick from the live fleet probe and [`super::DataSource`]; never writes.
//! Per-runner local state is keyed by install dir, since agentId is unique only
//! within an org; GitHub's view joins back by `(org, agent_id)`.

use std::collections::HashMap;
use std::time::Instant;

use crate::shared::collectors;
use crate::shared::hooks::install;
use crate::shared::models::{BusyPoint, GhView, HistPoint, HostPoint, Liveness, RunnerState};
use crate::shared::paths::Scope;
use crate::shared::util::now_epoch;

use super::{App, HISTORY_POINTS, JOB_ROWS, LiveRunner, TREND_POINTS, Tab};

impl App {
    pub(crate) fn refresh(&mut self) {
        let now = now_epoch();
        // `walk_work=false`: the _work total is slow to walk; history supplies it.
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

        self.source.reconnect_if_ephemeral();
        let api = self.source.latest_api_runners();
        let persisted = self.source.runner_states();
        let orgs = self.source.configured_token_orgs();
        self.configured_orgs = orgs.unwrap_or_else(|| {
            self.cfg
                .github
                .tokens
                .keys()
                .map(ToString::to_string)
                .collect()
        });
        // Hooks install System-scope but the dashboard usually runs non-root, so
        // any scope's hooks dir counts as ours.
        let our_dirs = [
            install::hooks_dir(&Scope::System.data_dir()),
            install::hooks_dir(&Scope::User.data_dir()),
        ];

        let mut edges = HashMap::with_capacity(snap.runners.len());
        let mut runners = Vec::with_capacity(snap.runners.len());
        let (mut busy, mut online) = (0u32, 0u32);
        for p in snap.runners {
            let id = p.info.agent_id;
            let dirkey = p.info.dir.to_string_lossy().into_owned();
            let cpu_pct = self.cpu.rate(&p.info.dir, p.cpu_usage_usec, sampled_at);
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
            let edge_since = match self.edges.get(&dirkey) {
                Some((prev, since)) if *prev == p.liveness => *since,
                _ => now,
            };
            edges.insert(dirkey.clone(), (p.liveness, edge_since));
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
                hook: install::detect(&p.info.dir, &our_dirs),
                work_folder: p.info.work_folder,
                agent_id: id,
                name: p.info.name,
                org: p.info.org,
                scope: p.info.scope,
                group: p.info.group,
                dir: p.info.dir,
                user: p.info.user,
            });
        }
        self.rings.push_busy(BusyPoint {
            ts: now,
            busy,
            online,
            // Ephemeral has no reconcile: `None` plots a gap; 0 would claim nothing is online.
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

/// The collector's persisted edge survives TUI restarts but may lag a live transition.
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
        assert_eq!(pick_since(Some(&persisted), Liveness::Busy, 900), 100);
    }

    #[test]
    fn pick_since_falls_back_on_disagreement_or_absence() {
        let persisted = state(Liveness::Idle, 100);
        assert_eq!(pick_since(Some(&persisted), Liveness::Busy, 900), 900);
        assert_eq!(pick_since(None, Liveness::Busy, 900), 900);
    }
}
