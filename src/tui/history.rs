//! The TUI's two history sources, behind one enum.
//!
//! - **Ephemeral**: no collector reachable. History comes from [`Rings`] — a
//!   bounded in-memory buffer the App fills from its own live sample each tick,
//!   so Trends + Detail sparklines show a rolling since-launch window. Nothing
//!   persists; GitHub + Jobs (collector-only features) are simply empty.
//! - **Persistent**: a collector is reachable over the IPC socket. History,
//!   Jobs, and the GitHub view are fetched from it; the rings still fill every
//!   tick as a warm fallback if the socket drops mid-session.
//!
//! Mode is not a stored flag — it is `matches!(source, Persistent)`. A failed
//! IPC request reverts the source to Ephemeral in place, and `App::refresh`
//! re-probes when Ephemeral, so a collector starting or stopping while the TUI
//! is open needs no special handling.

use std::collections::{HashMap, VecDeque};

use crate::shared::ipc::{Mutation, Query, Request, Response};
use crate::shared::models::{ApiState, BusyPoint, HistPoint, HostPoint, JobRow, RunnerState};
use crate::shared::paths::Scope;
use crate::tui::ipc_client::{self, Client};

/// Which data plane the TUI is on. Drives the header badge + Config tab. A pure
/// data enum — how it is rendered (label, colour) lives in `viewmodel::style`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Ephemeral,
    Persistent,
}

/// Result of a config mutation attempted over the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutateOutcome {
    /// The authorized collector persisted the change.
    Mutated,
    /// The collector refused — the peer isn't root or in the `ghr-stats` group.
    Denied,
    /// No collector reachable (Ephemeral) or the request errored — caller falls
    /// back to a direct write.
    Unreachable,
}

/// The App's history source: an in-memory ring buffer, or a live collector.
pub(crate) enum DataSource {
    Ephemeral,
    Persistent(Client),
}

impl DataSource {
    /// Probe for a reachable collector (System scope, then User). A hit ⇒
    /// Persistent; otherwise Ephemeral.
    pub(crate) fn detect() -> Self {
        match Client::connect_any() {
            Some(c) => DataSource::Persistent(c),
            None => DataSource::Ephemeral,
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        match self {
            DataSource::Persistent(_) => Mode::Persistent,
            DataSource::Ephemeral => Mode::Ephemeral,
        }
    }

    /// The scope of the connected collector (for the Config tab to note when it
    /// differs from the TUI's own scope). `None` in Ephemeral mode.
    pub(crate) fn scope(&self) -> Option<Scope> {
        match self {
            DataSource::Persistent(c) => Some(c.scope()),
            DataSource::Ephemeral => None,
        }
    }

    /// When Ephemeral, try once to attach to a collector that has since started.
    pub(crate) fn reconnect_if_ephemeral(&mut self) {
        if matches!(self, DataSource::Ephemeral)
            && let Some(c) = Client::connect_any()
        {
            *self = DataSource::Persistent(c);
        }
    }

    /// One IPC round-trip. `None` in Ephemeral mode, or if the request fails —
    /// in which case the source reverts to Ephemeral (the App re-probes next tick).
    fn query(&mut self, req: &Request) -> Option<Response> {
        let DataSource::Persistent(client) = self else {
            return None;
        };
        match client.request(req) {
            Ok(resp) => Some(resp),
            Err(e) => {
                tracing::debug!(error = %e, "ipc request failed — reverting to Ephemeral");
                *self = DataSource::Ephemeral;
                None
            }
        }
    }

    // --- typed queries: IPC in Persistent mode, ring / empty fallback otherwise ---

    pub(crate) fn latest_api_runners(&mut self) -> HashMap<i64, ApiState> {
        match self.query(&Request::Query(Query::LatestApiRunners)) {
            Some(Response::LatestApiRunners(rows)) => ipc_client::api_map(rows),
            _ => HashMap::new(), // GitHub is Persistent-only
        }
    }

    /// Persisted per-runner liveness edges (Persistent only) — the true, restart-
    /// surviving `since_ts` for the "For" duration. Empty in Ephemeral mode, where
    /// the App falls back to its in-memory edge.
    pub(crate) fn runner_states(&mut self) -> HashMap<i64, RunnerState> {
        match self.query(&Request::Query(Query::RunnerStates)) {
            Some(Response::RunnerStates(rows)) => {
                rows.into_iter().map(|st| (st.agent_id, st)).collect()
            }
            _ => HashMap::new(),
        }
    }

    pub(crate) fn host_series(&mut self, rings: &Rings, limit: usize) -> Vec<HostPoint> {
        match self.query(&Request::Query(Query::HostSeries { limit })) {
            Some(Response::HostSeries(v)) => v,
            _ => rings.host_series(limit),
        }
    }

    pub(crate) fn busy_series(&mut self, rings: &Rings, limit: usize) -> Vec<BusyPoint> {
        match self.query(&Request::Query(Query::BusySeries { limit })) {
            Some(Response::BusySeries(v)) => v,
            _ => rings.busy_series(limit),
        }
    }

    pub(crate) fn runner_history(
        &mut self,
        rings: &Rings,
        id: i64,
        limit: usize,
    ) -> Vec<HistPoint> {
        match self.query(&Request::Query(Query::RunnerHistory {
            agent_id: id,
            limit,
        })) {
            Some(Response::RunnerHistory(v)) => v,
            _ => rings.runner_history(id, limit),
        }
    }

    /// The configured token org logins as seen by the collector (which reads the
    /// root-owned /etc config) — presence only, no token values. `None` in
    /// Ephemeral mode or on error, so the caller falls back to its own loaded cfg.
    pub(crate) fn configured_token_orgs(&mut self) -> Option<Vec<String>> {
        match self.query(&Request::Query(Query::ConfiguredTokenOrgs)) {
            Some(Response::ConfiguredTokenOrgs(orgs)) => Some(orgs),
            _ => None,
        }
    }

    pub(crate) fn recent_jobs(&mut self, limit: usize) -> Vec<JobRow> {
        match self.query(&Request::Query(Query::RecentJobs { limit })) {
            Some(Response::RecentJobs(v)) => v,
            _ => Vec::new(), // Jobs are Persistent-only
        }
    }

    pub(crate) fn latest_job(&mut self, runner_name: &str) -> Option<JobRow> {
        match self.query(&Request::Query(Query::LatestJob {
            runner_name: runner_name.to_string(),
        })) {
            Some(Response::LatestJob(j)) => j,
            _ => None, // Persistent-only
        }
    }

    // --- authorized mutations (the collector writes /etc on our behalf) ---

    pub(crate) fn set_metrics_pull(&mut self, enabled: bool, addr: &str) -> MutateOutcome {
        self.mutate(Request::Mutate(Mutation::SetMetricsPull {
            enabled,
            addr: addr.to_string(),
        }))
    }

    pub(crate) fn add_org_token(&mut self, org: &str, token: &str) -> MutateOutcome {
        self.mutate(Request::Mutate(Mutation::AddOrgToken {
            org: org.to_string(),
            token: token.to_string(),
        }))
    }

    pub(crate) fn remove_org_token(&mut self, org: &str) -> MutateOutcome {
        self.mutate(Request::Mutate(Mutation::RemoveOrgToken {
            org: org.to_string(),
        }))
    }

    /// Send a mutation request; map the reply. `Unreachable` when Ephemeral or on
    /// any error (the caller then falls back to a direct write).
    fn mutate(&mut self, req: Request) -> MutateOutcome {
        match self.query(&req) {
            Some(Response::Mutated) => MutateOutcome::Mutated,
            Some(Response::Denied) => MutateOutcome::Denied,
            _ => MutateOutcome::Unreachable,
        }
    }
}

/// Bounded, in-memory history for Ephemeral mode. Fed from the App's live sample
/// each tick; capped so it is O(1) memory and reflects a rolling window.
pub(crate) struct Rings {
    host: VecDeque<HostPoint>,
    busy: VecDeque<BusyPoint>,
    runners: HashMap<i64, VecDeque<HistPoint>>,
    trend_cap: usize,
    hist_cap: usize,
}

impl Rings {
    pub(crate) fn new(trend_cap: usize, hist_cap: usize) -> Self {
        Self {
            host: VecDeque::new(),
            busy: VecDeque::new(),
            runners: HashMap::new(),
            trend_cap,
            hist_cap,
        }
    }

    pub(crate) fn push_host(&mut self, p: HostPoint) {
        push_capped(&mut self.host, p, self.trend_cap);
    }

    pub(crate) fn push_busy(&mut self, p: BusyPoint) {
        push_capped(&mut self.busy, p, self.trend_cap);
    }

    pub(crate) fn push_runner(&mut self, id: i64, p: HistPoint) {
        let cap = self.hist_cap;
        push_capped(self.runners.entry(id).or_default(), p, cap);
    }

    /// Newest `limit` points, oldest → newest — matching `store::reader`'s order.
    fn host_series(&self, limit: usize) -> Vec<HostPoint> {
        tail(&self.host, limit)
    }

    fn busy_series(&self, limit: usize) -> Vec<BusyPoint> {
        tail(&self.busy, limit)
    }

    fn runner_history(&self, id: i64, limit: usize) -> Vec<HistPoint> {
        self.runners
            .get(&id)
            .map(|dq| tail(dq, limit))
            .unwrap_or_default()
    }
}

/// Push, evicting the oldest when at capacity (`cap >= 1`).
fn push_capped<T>(dq: &mut VecDeque<T>, item: T, cap: usize) {
    if dq.len() >= cap {
        dq.pop_front();
    }
    dq.push_back(item);
}

/// The last `limit` items, cloned in order.
fn tail<T: Clone>(dq: &VecDeque<T>, limit: usize) -> Vec<T> {
    let start = dq.len().saturating_sub(limit);
    dq.iter().skip(start).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(ts: i64) -> HostPoint {
        HostPoint {
            ts,
            load1: 1.0,
            mem_used: 1,
            mem_total: 2,
            tmp_bytes: None,
            work_bytes: None,
            root_free: None,
        }
    }

    #[test]
    fn rings_cap_and_return_newest_oldest_first() {
        let mut r = Rings::new(3, 2);
        for ts in [10, 20, 30, 40] {
            r.push_host(host(ts));
        }
        // capped at 3 ⇒ oldest (10) evicted; oldest → newest
        assert_eq!(
            r.host_series(10).iter().map(|h| h.ts).collect::<Vec<_>>(),
            vec![20, 30, 40]
        );
        // limit smaller than contents ⇒ newest `limit`
        assert_eq!(
            r.host_series(2).iter().map(|h| h.ts).collect::<Vec<_>>(),
            vec![30, 40]
        );
    }

    #[test]
    fn per_runner_history_is_independent_and_capped() {
        let mut r = Rings::new(3, 2);
        for ts in [1, 2, 3] {
            r.push_runner(
                7,
                HistPoint {
                    ts,
                    cpu_pct: None,
                    mem_bytes: None,
                },
            );
        }
        // hist_cap = 2 ⇒ ts 1 evicted
        assert_eq!(
            r.runner_history(7, 5)
                .iter()
                .map(|p| p.ts)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(r.runner_history(999, 5).is_empty());
    }

    #[test]
    fn ephemeral_source_has_no_persistent_data() {
        let mut s = DataSource::Ephemeral;
        assert_eq!(s.mode(), Mode::Ephemeral);
        assert!(s.recent_jobs(10).is_empty());
        assert!(s.latest_job("r").is_none());
        assert!(s.latest_api_runners().is_empty());
        assert!(s.scope().is_none());
    }
}
