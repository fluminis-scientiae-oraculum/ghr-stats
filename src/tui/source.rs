//! History from in-memory [`Rings`] (Ephemeral) or the collector over IPC (Persistent).
//! A failed request reverts to Ephemeral in place; `App::refresh` re-probes each tick.

use std::collections::{HashMap, VecDeque};

use crate::shared::ipc::client::{self as ipc_client, Client, EphemeralReason};
use crate::shared::ipc::{Mutation, Query, Request, Response};
use crate::shared::models::{BusyPoint, GhView, HistPoint, HostPoint, JobRow, Mode, RunnerState};
use crate::shared::paths::Scope;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutateOutcome {
    Mutated,
    /// The peer is neither root nor in the `ghr-stats` group.
    Denied,
    /// Ephemeral, or the request failed; the caller writes the file directly.
    Unreachable,
}

pub(crate) enum DataSource {
    Ephemeral(EphemeralReason),
    Persistent(Client),
}

impl DataSource {
    pub(crate) fn detect() -> Self {
        match Client::connect_any() {
            Ok(c) => DataSource::Persistent(c),
            Err(reason) => DataSource::Ephemeral(reason),
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        match self {
            DataSource::Persistent(_) => Mode::Persistent,
            DataSource::Ephemeral(_) => Mode::Ephemeral,
        }
    }

    pub(crate) fn scope(&self) -> Option<Scope> {
        match self {
            DataSource::Persistent(c) => Some(c.scope()),
            DataSource::Ephemeral(_) => None,
        }
    }

    pub(crate) fn collector_version(&self) -> Option<&str> {
        match self {
            DataSource::Persistent(c) => c.collector_version(),
            DataSource::Ephemeral(_) => None,
        }
    }

    pub(crate) fn ephemeral_reason(&self) -> Option<&EphemeralReason> {
        match self {
            DataSource::Ephemeral(r) => Some(r),
            DataSource::Persistent(_) => None,
        }
    }

    pub(crate) fn reconnect_if_ephemeral(&mut self) {
        if matches!(self, DataSource::Ephemeral(_)) {
            match Client::connect_any() {
                Ok(c) => *self = DataSource::Persistent(c),
                // A collector restarted onto a matching wire version is no longer drifted.
                Err(reason) => *self = DataSource::Ephemeral(reason),
            }
        }
    }

    fn query(&mut self, req: &Request) -> Option<Response> {
        let DataSource::Persistent(client) = self else {
            return None;
        };
        match client.request(req) {
            Ok(resp) => Some(resp),
            Err(e) => {
                tracing::debug!(error = %e, "ipc request failed — reverting to Ephemeral");
                *self = DataSource::Ephemeral(EphemeralReason::NoCollector);
                None
            }
        }
    }

    pub(crate) fn latest_api_runners(&mut self) -> HashMap<(String, i64), GhView> {
        match self.query(&Request::Query(Query::LatestApiRunners)) {
            Some(Response::LatestApiRunners(rows)) => ipc_client::api_map(rows),
            _ => HashMap::new(),
        }
    }

    pub(crate) fn runner_states(&mut self) -> HashMap<String, RunnerState> {
        match self.query(&Request::Query(Query::RunnerStates)) {
            Some(Response::RunnerStates(rows)) => {
                rows.into_iter().map(|st| (st.dir.clone(), st)).collect()
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
        dir: &str,
        limit: usize,
    ) -> Vec<HistPoint> {
        match self.query(&Request::Query(Query::RunnerHistory {
            dir: dir.to_string(),
            limit,
        })) {
            Some(Response::RunnerHistory(v)) => v,
            _ => rings.runner_history(dir, limit),
        }
    }

    pub(crate) fn configured_token_orgs(&mut self) -> Option<Vec<String>> {
        match self.query(&Request::Query(Query::ConfiguredTokenOrgs)) {
            Some(Response::ConfiguredTokenOrgs(orgs)) => Some(orgs),
            _ => None,
        }
    }

    pub(crate) fn recent_jobs(&mut self, limit: usize) -> Vec<JobRow> {
        match self.query(&Request::Query(Query::RecentJobs { limit })) {
            Some(Response::RecentJobs(v)) => v,
            _ => Vec::new(),
        }
    }

    pub(crate) fn latest_job(&mut self, runner_name: &str) -> Option<JobRow> {
        match self.query(&Request::Query(Query::LatestJob {
            runner_name: runner_name.to_string(),
        })) {
            Some(Response::LatestJob(j)) => j,
            _ => None,
        }
    }

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

    fn mutate(&mut self, req: Request) -> MutateOutcome {
        match self.query(&req) {
            Some(Response::Mutated) => MutateOutcome::Mutated,
            Some(Response::Denied) => MutateOutcome::Denied,
            _ => MutateOutcome::Unreachable,
        }
    }
}

pub(crate) struct Rings {
    host: VecDeque<HostPoint>,
    busy: VecDeque<BusyPoint>,
    runners: HashMap<String, VecDeque<HistPoint>>,
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

    pub(crate) fn push_runner(&mut self, dir: String, p: HistPoint) {
        let cap = self.hist_cap;
        push_capped(self.runners.entry(dir).or_default(), p, cap);
    }

    /// Oldest → newest, matching `store::reader`'s order.
    fn host_series(&self, limit: usize) -> Vec<HostPoint> {
        tail(&self.host, limit)
    }

    fn busy_series(&self, limit: usize) -> Vec<BusyPoint> {
        tail(&self.busy, limit)
    }

    fn runner_history(&self, dir: &str, limit: usize) -> Vec<HistPoint> {
        self.runners
            .get(dir)
            .map(|dq| tail(dq, limit))
            .unwrap_or_default()
    }
}

/// Requires `cap >= 1`.
fn push_capped<T>(dq: &mut VecDeque<T>, item: T, cap: usize) {
    if dq.len() >= cap {
        dq.pop_front();
    }
    dq.push_back(item);
}

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
        assert_eq!(
            r.host_series(10).iter().map(|h| h.ts).collect::<Vec<_>>(),
            vec![20, 30, 40]
        );
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
                "/srv/r7".to_string(),
                HistPoint {
                    ts,
                    cpu_pct: None,
                    mem_bytes: None,
                },
            );
        }
        assert_eq!(
            r.runner_history("/srv/r7", 5)
                .iter()
                .map(|p| p.ts)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(r.runner_history("/srv/none", 5).is_empty());
    }
}
