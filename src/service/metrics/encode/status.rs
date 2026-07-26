//! What the wire gets: the snapshot adjudicated into one answer.
//!
//! The only projection that computes a [`Verdict`], and the only one whose types
//! cross the IPC boundary — which is the same statement twice, because a verdict
//! is what a remote caller cannot derive for itself.
//!
//! It is computed on the COLLECTOR rather than left to each consumer: an agent
//! re-deriving "is this healthy?" from six gauges gets it wrong the same way a
//! human does, which is the whole lesson of the incident this release addresses.
//! One verdict, one place. `ipc_server::dispatch` is the sole caller.

use crate::shared::models::{FleetCounts, FleetStatus, Mode, OrgStatus, RunnerStatus, Verdict};

use super::Snapshot;

impl Snapshot {
    /// Project the snapshot into the machine-facing [`FleetStatus`] payload.
    ///
    /// The verdict is computed HERE, on the collector, rather than left to each
    /// consumer: an agent re-deriving "is this healthy?" from six gauges gets it
    /// wrong the same way a human does — which is the whole lesson of the
    /// incident this release addresses. One verdict, one place.
    pub fn to_status(&self, mode: Mode) -> FleetStatus {
        let reconcile_age = |org: &str| -> Option<i64> {
            self.reconcile
                .iter()
                .find(|c| c.org == org)
                .and_then(|c| c.last_ok_ts)
                .map(|ts| (self.now - ts).max(0))
        };

        let runners: Vec<RunnerStatus> = self
            .runners
            .iter()
            .map(|r| RunnerStatus {
                name: r.name.clone(),
                org: r.org.clone(),
                agent_id: r.agent_id,
                liveness: r.liveness,
                state_seconds: r.state_seconds,
                github_online: r.gh.online(),
                github_busy: r.gh.busy(),
                github_offline_seconds: r.gh_offline_seconds,
                github_sample_age_s: r.gh.age_s(),
                divergent: r.divergent(),
                cpu_percent: r.cpu_pct,
                mem_bytes: r.mem_bytes,
            })
            .collect();

        let orgs = self
            .orgs
            .iter()
            .map(|o| {
                // An org is degraded when GitHub sees fewer runners online than
                // this host has for it — the shape the incident took.
                let verdict = if o.github_online < o.total {
                    Verdict::Degraded
                } else {
                    Verdict::Ok
                };
                OrgStatus {
                    org: o.org.clone(),
                    runners: o.total,
                    github_online: o.github_online,
                    reconcile_age_s: reconcile_age(&o.org),
                    verdict,
                }
            })
            .collect();

        // "No runners at all" is not health — it is an inability to answer, and
        // must not exit 0 as though the fleet were fine.
        let verdict = if self.runners.is_empty() {
            Verdict::Unknown
        } else if self.divergent > 0 || self.offline > 0 {
            Verdict::Degraded
        } else {
            Verdict::Ok
        };

        FleetStatus {
            schema_version: 1,
            generated_at: crate::shared::util::to_rfc3339_utc(self.now),
            generated_at_epoch: self.now,
            mode,
            verdict,
            fleet: FleetCounts {
                runners: self.runners.len() as u32,
                busy: self.busy,
                idle: self.idle,
                offline: self.offline,
                divergent: self.divergent,
            },
            orgs,
            runners,
        }
    }
}
