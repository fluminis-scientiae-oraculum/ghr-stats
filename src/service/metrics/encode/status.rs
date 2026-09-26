//! Adjudicates a [`Snapshot`] into the [`FleetStatus`] sent over IPC, so consumers share
//! one verdict.

use crate::shared::models::{FleetCounts, FleetStatus, Mode, OrgStatus, RunnerStatus, Verdict};

use super::Snapshot;

impl Snapshot {
    /// No runners is an inability to answer, not health.
    pub fn verdict(&self) -> Verdict {
        if self.runners.is_empty() {
            Verdict::Unknown
        } else if self.divergent > 0 || self.offline > 0 {
            Verdict::Degraded
        } else {
            Verdict::Ok
        }
    }

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
                // Only fresh readings count: an org we cannot ask about (no PAT, broken
                // token, reconcile gap) is Unknown, not an outage.
                let verdict = if o.github_known == 0 {
                    Verdict::Unknown
                } else if o.github_online < o.github_known {
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

        let verdict = self.verdict();

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn seed_unreconciled() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut c);
        c.execute(
            "INSERT INTO runner_sample (ts,agent_id,name,org,liveness,dir) \
             VALUES (1000,1,'r1','acme','idle','/srv/r1')",
            [],
        )
        .unwrap();
        c
    }

    #[test]
    fn an_org_we_cannot_ask_about_is_unknown_not_degraded() {
        let snap = Snapshot::gather(&seed_unreconciled(), 1100, "9.9.9", 180).unwrap();
        let st = snap.to_status(Mode::Persistent);
        let org = &st.orgs[0];

        assert_eq!(org.runners, 1);
        assert_eq!(org.github_online, 0);
        assert_eq!(org.reconcile_age_s, None, "never reconciled");

        assert_eq!(
            org.verdict,
            Verdict::Unknown,
            "an org with no successful reconcile is UNANSWERABLE, not failing"
        );
    }

    #[test]
    fn a_runner_we_can_see_going_offline_still_degrades_its_org() {
        let mut c = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut c);
        for (id, name) in [(1, "r1"), (2, "r2")] {
            c.execute(
                "INSERT INTO runner_sample (ts,agent_id,name,org,liveness,dir) \
                 VALUES (1000,?1,?2,'acme','idle',?3)",
                rusqlite::params![id, name, format!("/srv/{name}")],
            )
            .unwrap();
        }
        for (id, online) in [(1, 0), (2, 1)] {
            c.execute(
                "INSERT INTO api_runner_sample (ts,agent_id,org,name,online,busy) \
                 VALUES (1000,?1,'acme','r',?2,0)",
                rusqlite::params![id, online],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO api_reconcile_state \
                 (org,last_ok_ts,last_try_ts,ok,http_status,error_kind,configured) \
             VALUES ('acme',1000,1000,1,NULL,NULL,1)",
            [],
        )
        .unwrap();

        let org = &Snapshot::gather(&c, 1100, "9.9.9", 180)
            .unwrap()
            .to_status(Mode::Persistent)
            .orgs[0];
        assert_eq!((org.runners, org.github_online), (2, 1));
        assert_eq!(
            org.verdict,
            Verdict::Degraded,
            "a visible runner that GitHub says is offline must still degrade"
        );
    }
}
