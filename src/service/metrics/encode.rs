//! Metric gathering + rendering. Reads the store on a caller-provided
//! connection, builds a [`Snapshot`], and renders it two ways: Prometheus text
//! exposition (for the pull endpoint) and a flat JSON array (for the push
//! sink). Pure reads + formatting — no DB writes, no I/O of its own.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::error::Result;
use crate::shared::models::{self, ApiReconcileState, GhView, Liveness};

/// One runner's metric row.
struct RunnerMetric {
    agent_id: i64,
    name: String,
    org: String,
    liveness: Liveness,
    cpu_pct: Option<f32>,
    mem_bytes: Option<u64>,
    mem_current_bytes: Option<u64>,
    /// Seconds in the current liveness state (`now - since_ts`).
    state_seconds: i64,
    /// GitHub's view, freshness already adjudicated by the reader. Held as the
    /// whole verdict rather than pre-flattened booleans so the exporter can
    /// distinguish "GitHub says offline" from "we have no current reading" —
    /// the distinction the all-green rollup was missing.
    gh: GhView,
    /// Seconds this runner has been continuously offline *to GitHub*, from the
    /// persisted edge. `None` when online, or when there is no edge yet.
    gh_offline_seconds: Option<i64>,
}

impl RunnerMetric {
    /// The common `agent_id`/`name`/`org` label set, escaped.
    fn labels(&self) -> String {
        format!(
            "agent_id=\"{}\",name=\"{}\",org=\"{}\"",
            self.agent_id,
            esc(&self.name),
            esc(&self.org)
        )
    }

    /// See [`models::divergent`] — the one place this verdict is derived, shared
    /// with the TUI header so the exporter and the dashboard cannot disagree.
    fn divergent(&self) -> Option<bool> {
        models::divergent(self.liveness, self.gh)
    }
}

/// Per-org rollup. The 62-vs-0/0/0 arithmetic across orgs is what turned "is
/// this abnormal?" into a decidable question during the incident, so it is
/// exported rather than left for a query to reconstruct.
struct OrgRollup {
    org: String,
    total: u32,
    github_online: u32,
}

/// A point-in-time metrics snapshot, gathered once per scrape/push.
pub struct Snapshot {
    version: String,
    now: i64,
    last_sample_ts: Option<i64>,
    runners: Vec<RunnerMetric>,
    busy: u32,
    idle: u32,
    offline: u32,
    load1: Option<f64>,
    mem_used: Option<u64>,
    mem_total: Option<u64>,
    jobs_total: i64,
    jobs_running: i64,
    /// Runners that are locally up while GitHub says they cannot take work.
    divergent: u32,
    orgs: Vec<OrgRollup>,
    reconcile: Vec<ApiReconcileState>,
    /// The configured freshness window, exported so a scrape is self-describing
    /// and an alert can reference the operator's value instead of hardcoding it.
    max_age: u64,
}

impl Snapshot {
    /// Read the current fleet state into a snapshot. `max_age` bounds how old a
    /// GitHub reconcile row may be and still count as current — see
    /// [`crate::shared::config::Intervals::api_max_age`], the one place it is
    /// decided.
    pub fn gather(conn: &Connection, now: i64, version: &str, max_age: u64) -> Result<Snapshot> {
        let latest = reader::latest_runners(conn)?;
        let states = reader::runner_states(conn)?;
        let api = reader::latest_api_runners(conn, now, max_age)?;
        let api_edges = reader::api_runner_states(conn)?;
        let reconcile = reader::api_reconcile_states(conn)?;
        let host = reader::latest_host(conn)?;
        let (jobs_total, jobs_running) = reader::job_counts(conn)?;

        let last_sample_ts = latest.iter().map(|r| r.ts).max();
        let (mut busy, mut idle, mut offline) = (0u32, 0u32, 0u32);
        let runners = latest
            .into_iter()
            .map(|r| {
                match r.liveness {
                    Liveness::Busy => busy += 1,
                    Liveness::Idle => idle += 1,
                    Liveness::Offline => offline += 1,
                }
                let state_seconds = states
                    .get(&r.dir)
                    .map(|s| (now - s.since_ts).max(0))
                    .unwrap_or(0);
                // A runner with no reconcile row at all is Unknown — never
                // silently folded into "offline".
                let gh = api
                    .get(&(r.org.clone(), r.agent_id))
                    .copied()
                    .unwrap_or(GhView::Unknown);
                // Offline duration comes from the persisted edge, not from the
                // instantaneous bit: it survives collector restarts and scrape
                // gaps, which is the same reason `runner_state` exists locally.
                let gh_offline_seconds = api_edges
                    .get(&(r.org.clone(), r.agent_id))
                    .filter(|e| !e.online)
                    .map(|e| (now - e.since_ts).max(0));
                RunnerMetric {
                    agent_id: r.agent_id,
                    name: r.name,
                    org: r.org,
                    liveness: r.liveness,
                    cpu_pct: r.cpu_pct,
                    mem_bytes: r.mem_bytes,
                    mem_current_bytes: r.mem_current_bytes,
                    state_seconds,
                    gh,
                    gh_offline_seconds,
                }
            })
            .collect::<Vec<RunnerMetric>>();

        let divergent = runners
            .iter()
            .filter(|r| r.divergent() == Some(true))
            .count() as u32;

        // Per-org totals. Only FRESH readings count as online — a stale row must
        // not inflate an org's health.
        let mut by_org: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
        for r in &runners {
            let e = by_org.entry(r.org.as_str()).or_default();
            e.0 += 1;
            if r.gh.online() == Some(true) {
                e.1 += 1;
            }
        }
        let orgs = by_org
            .into_iter()
            .map(|(org, (total, github_online))| OrgRollup {
                org: org.to_string(),
                total,
                github_online,
            })
            .collect();

        Ok(Snapshot {
            version: version.to_string(),
            now,
            last_sample_ts,
            runners,
            busy,
            idle,
            offline,
            load1: host.as_ref().map(|h| h.load1),
            mem_used: host.as_ref().map(|h| h.mem_used),
            mem_total: host.as_ref().map(|h| h.mem_total),
            jobs_total,
            jobs_running,
            divergent,
            orgs,
            reconcile,
            max_age,
        })
    }

    /// Render the Prometheus text exposition (format 0.0.4).
    pub fn to_prometheus(&self) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(2048);

        let _ = writeln!(
            s,
            "# HELP ghr_build_info Build metadata.\n\
             # TYPE ghr_build_info gauge\n\
             ghr_build_info{{version=\"{}\"}} 1",
            esc(&self.version)
        );

        let _ = writeln!(
            s,
            "# TYPE ghr_fleet_runners gauge\nghr_fleet_runners {}\n\
             # TYPE ghr_fleet_by_state gauge\n\
             ghr_fleet_by_state{{state=\"busy\"}} {}\n\
             ghr_fleet_by_state{{state=\"idle\"}} {}\n\
             ghr_fleet_by_state{{state=\"offline\"}} {}\n\
             ghr_fleet_by_state{{state=\"divergent\"}} {}",
            self.runners.len(),
            self.busy,
            self.idle,
            self.offline,
            self.divergent,
        );
        // NOTE: `divergent` CROSS-CUTS busy/idle/offline rather than partitioning
        // them — a divergent runner is also counted as idle or busy. The three
        // original values keep their exact prior meaning so existing dashboards
        // do not shift under an upgrade; summing all four would double-count.

        if let Some(ts) = self.last_sample_ts {
            let _ = writeln!(
                s,
                "# TYPE ghr_last_sample_timestamp_seconds gauge\n\
                 ghr_last_sample_timestamp_seconds {ts}"
            );
        }
        if let Some(v) = self.load1 {
            let _ = writeln!(s, "# TYPE ghr_host_load1 gauge\nghr_host_load1 {v}");
        }
        if let (Some(u), Some(t)) = (self.mem_used, self.mem_total) {
            let _ = writeln!(
                s,
                "# TYPE ghr_host_mem_bytes gauge\n\
                 ghr_host_mem_bytes{{kind=\"used\"}} {u}\n\
                 ghr_host_mem_bytes{{kind=\"total\"}} {t}"
            );
        }
        let _ = writeln!(
            s,
            "# TYPE ghr_jobs_total gauge\nghr_jobs_total {}\n\
             # TYPE ghr_jobs_running gauge\nghr_jobs_running {}",
            self.jobs_total, self.jobs_running,
        );

        let _ = writeln!(s, "# TYPE ghr_runner_up gauge");
        for r in &self.runners {
            let up = i32::from(r.liveness != Liveness::Offline);
            let _ = writeln!(s, "ghr_runner_up{{{}}} {up}", r.labels());
        }
        let _ = writeln!(s, "# TYPE ghr_runner_busy gauge");
        for r in &self.runners {
            let b = i32::from(r.liveness == Liveness::Busy);
            let _ = writeln!(s, "ghr_runner_busy{{{}}} {b}", r.labels());
        }
        let _ = writeln!(s, "# TYPE ghr_runner_cpu_percent gauge");
        for r in &self.runners {
            if let Some(c) = r.cpu_pct {
                let _ = writeln!(s, "ghr_runner_cpu_percent{{{}}} {c}", r.labels());
            }
        }
        let _ = writeln!(s, "# TYPE ghr_runner_mem_bytes gauge");
        for r in &self.runners {
            if let Some(m) = r.mem_bytes {
                let _ = writeln!(s, "ghr_runner_mem_bytes{{{}}} {m}", r.labels());
            }
        }
        let _ = writeln!(s, "# TYPE ghr_runner_mem_current_bytes gauge");
        for r in &self.runners {
            if let Some(m) = r.mem_current_bytes {
                let _ = writeln!(s, "ghr_runner_mem_current_bytes{{{}}} {m}", r.labels());
            }
        }
        let _ = writeln!(s, "# TYPE ghr_runner_state_seconds gauge");
        for r in &self.runners {
            let _ = writeln!(
                s,
                "ghr_runner_state_seconds{{{},state=\"{}\"}} {}",
                r.labels(),
                r.liveness.as_str(),
                r.state_seconds,
            );
        }
        let _ = writeln!(s, "# TYPE ghr_runner_github_online gauge");
        for r in &self.runners {
            if let Some(o) = r.gh.online() {
                let _ = writeln!(
                    s,
                    "ghr_runner_github_online{{{}}} {}",
                    r.labels(),
                    i32::from(o)
                );
            }
        }
        let _ = writeln!(s, "# TYPE ghr_runner_github_busy gauge");
        for r in &self.runners {
            if let Some(b) = r.gh.busy() {
                let _ = writeln!(
                    s,
                    "ghr_runner_github_busy{{{}}} {}",
                    r.labels(),
                    i32::from(b)
                );
            }
        }

        // How old each GitHub reading is. Lets a scrape watch staleness build,
        // rather than only seeing its aftermath.
        let _ = writeln!(s, "# TYPE ghr_runner_github_sample_age_seconds gauge");
        for r in &self.runners {
            if let Some(age) = r.gh.age_s() {
                let _ = writeln!(
                    s,
                    "ghr_runner_github_sample_age_seconds{{{}}} {age}",
                    r.labels()
                );
            }
        }

        // The alertable quantity. Zero when online; absent when there is no edge
        // yet. Debounce on THIS, never on the instantaneous bit — the latter
        // flapped 62 times in three hours during the incident this fixes.
        let _ = writeln!(s, "# TYPE ghr_runner_github_offline_seconds gauge");
        for r in &self.runners {
            let secs = match (r.gh_offline_seconds, r.gh.online()) {
                (Some(v), _) => Some(v),
                (None, Some(true)) => Some(0),
                (None, _) => None,
            };
            if let Some(v) = secs {
                let _ = writeln!(s, "ghr_runner_github_offline_seconds{{{}}} {v}", r.labels());
            }
        }

        let _ = writeln!(s, "# TYPE ghr_runner_divergent gauge");
        for r in &self.runners {
            if let Some(d) = r.divergent() {
                let _ = writeln!(s, "ghr_runner_divergent{{{}}} {}", r.labels(), i32::from(d));
            }
        }

        let _ = writeln!(s, "# TYPE ghr_org_runners gauge");
        for o in &self.orgs {
            let _ = writeln!(
                s,
                "ghr_org_runners{{org=\"{0}\",state=\"total\"}} {1}\n\
                 ghr_org_runners{{org=\"{0}\",state=\"github_online\"}} {2}",
                esc(&o.org),
                o.total,
                o.github_online,
            );
        }

        // Reconcile health. Without these, a dead reconcile presents as a calm
        // fleet and the two alerts above cannot be trusted.
        let _ = writeln!(
            s,
            "# TYPE ghr_api_max_age_seconds gauge\nghr_api_max_age_seconds {}\n\
             # TYPE ghr_api_reconcile_ok gauge\n\
             # TYPE ghr_api_org_configured gauge\n\
             # TYPE ghr_api_reconcile_timestamp_seconds gauge\n\
             # TYPE ghr_api_reconcile_errors_total counter",
            self.max_age
        );
        for c in &self.reconcile {
            let org = esc(&c.org);
            let _ = writeln!(
                s,
                "ghr_api_reconcile_ok{{org=\"{org}\"}} {}\n\
                 ghr_api_org_configured{{org=\"{org}\"}} {}",
                i32::from(c.ok),
                i32::from(c.configured),
            );
            if let Some(ts) = c.last_ok_ts {
                let _ = writeln!(
                    s,
                    "ghr_api_reconcile_timestamp_seconds{{org=\"{org}\"}} {ts}"
                );
            }
            if let Some(kind) = &c.error_kind {
                let _ = writeln!(
                    s,
                    "ghr_api_reconcile_errors_total{{org=\"{org}\",kind=\"{}\"}} 1",
                    esc(kind)
                );
            }
        }
        s
    }

    /// Render a flat JSON array (one fleet record + one per runner), shaped for
    /// OpenObserve's `_json` ingest. `_timestamp` is microseconds.
    pub fn to_json(&self) -> String {
        use serde_json::{Value, json};
        let ts_us = self.now * 1_000_000;
        let mut arr: Vec<Value> = Vec::with_capacity(self.runners.len() + 1);
        arr.push(json!({
            "_timestamp": ts_us,
            "kind": "fleet",
            "version": self.version,
            "runners": self.runners.len(),
            "busy": self.busy,
            "idle": self.idle,
            "offline": self.offline,
            "load1": self.load1,
            "mem_used": self.mem_used,
            "mem_total": self.mem_total,
            "jobs_total": self.jobs_total,
            "jobs_running": self.jobs_running,
            "last_sample_ts": self.last_sample_ts,
            "divergent": self.divergent,
            // Ship the verdict, not just the numbers. A consumer re-deriving
            // "is this healthy?" from the gauges gets it wrong the same way a
            // human does — the tool already knows local-up + GitHub-offline is
            // bad, so it says so.
            "verdict": if self.divergent > 0 || self.offline > 0 { "degraded" } else { "ok" },
            "orgs": self.orgs.iter().map(|o| json!({
                "org": o.org,
                "runners": o.total,
                "github_online": o.github_online,
            })).collect::<Vec<Value>>(),
        }));
        for r in &self.runners {
            arr.push(json!({
                "_timestamp": ts_us,
                "kind": "runner",
                "agent_id": r.agent_id,
                "name": r.name,
                "org": r.org,
                "liveness": r.liveness.as_str(),
                "up": i32::from(r.liveness != Liveness::Offline),
                "busy": i32::from(r.liveness == Liveness::Busy),
                "cpu_percent": r.cpu_pct,
                "mem_bytes": r.mem_bytes,
                "mem_current_bytes": r.mem_current_bytes,
                "state_seconds": r.state_seconds,
                "github_online": r.gh.online(),
                "github_busy": r.gh.busy(),
                "github_sample_age_s": r.gh.age_s(),
                "github_offline_seconds": r.gh_offline_seconds,
                "divergent": r.divergent(),
            }));
        }
        serde_json::to_string(&Value::Array(arr)).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Escape a Prometheus label value (`\`, `"`, newline).
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    fn seed() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::service::store::schema_for_test(&mut c);
        for (id, name, live) in [(1, "r1", "busy"), (2, "r2", "idle")] {
            c.execute(
                "INSERT INTO runner_sample (ts,agent_id,name,org,liveness,cpu_pct,mem_bytes,dir,mem_current_bytes) \
                 VALUES (1000,?1,?2,'acme',?3,12.5,1048576,?4,2097152)",
                params![id, name, live, format!("/srv/{name}")],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO runner_state (dir,liveness,since_ts,last_seen_ts) \
             VALUES ('/srv/r1','busy',900,1000)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO host_sample (ts,load1,load5,mem_used,mem_total) VALUES (1000,1.5,1.0,100,200)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO job_event (run_id,runner_name,started_at) VALUES (5,'r1',950)",
            [],
        )
        .unwrap();
        c
    }

    /// Seed a divergent fleet: both runners locally up, GitHub says r1 is
    /// offline (and has been since ts 500).
    fn seed_divergent() -> Connection {
        let c = seed();
        for (id, online) in [(1, 0), (2, 1)] {
            c.execute(
                "INSERT INTO api_runner_sample (ts,agent_id,org,name,online,busy) \
                 VALUES (1000,?1,'acme','r',?2,0)",
                params![id, online],
            )
            .unwrap();
            c.execute(
                "INSERT INTO api_runner_state (org,agent_id,online,since_ts,last_seen_ts) \
                 VALUES ('acme',?1,?2,?3,1000)",
                params![id, online, if online == 0 { 500 } else { 1000 }],
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
        c
    }

    /// The headline regression guard. `busy`/`idle`/`offline` must keep their
    /// exact prior values for a fleet with no divergence, so upgrading does not
    /// silently shift anyone's dashboards; `divergent` is an ADDITIONAL,
    /// cross-cutting label, not a fourth partition.
    #[test]
    fn fleet_by_state_is_unchanged_for_a_non_divergent_fleet() {
        let p = Snapshot::gather(&seed(), 1100, "9.9.9", 180)
            .unwrap()
            .to_prometheus();
        assert!(p.contains("ghr_fleet_by_state{state=\"busy\"} 1"));
        assert!(p.contains("ghr_fleet_by_state{state=\"idle\"} 1"));
        assert!(p.contains("ghr_fleet_by_state{state=\"offline\"} 0"));
        assert!(p.contains("ghr_fleet_by_state{state=\"divergent\"} 0"));
    }

    /// The state that mattered for four hours and had no representation.
    #[test]
    fn a_locally_up_github_offline_runner_is_divergent() {
        let p = Snapshot::gather(&seed_divergent(), 1100, "9.9.9", 180)
            .unwrap()
            .to_prometheus();
        assert!(p.contains("ghr_fleet_by_state{state=\"divergent\"} 1"));
        assert!(p.contains("ghr_runner_divergent{agent_id=\"1\",name=\"r1\",org=\"acme\"} 1"));
        assert!(p.contains("ghr_runner_divergent{agent_id=\"2\",name=\"r2\",org=\"acme\"} 0"));
        // Duration from the persisted edge: now(1100) - since_ts(500).
        assert!(p.contains(
            "ghr_runner_github_offline_seconds{agent_id=\"1\",name=\"r1\",org=\"acme\"} 600"
        ));
        // Org rollup — the arithmetic that made the incident obvious.
        assert!(p.contains("ghr_org_runners{org=\"acme\",state=\"total\"} 2"));
        assert!(p.contains("ghr_org_runners{org=\"acme\",state=\"github_online\"} 1"));
        // Reconcile health, so the alerts above are trustworthy.
        assert!(p.contains("ghr_api_reconcile_ok{org=\"acme\"} 1"));
        assert!(p.contains("ghr_api_reconcile_timestamp_seconds{org=\"acme\"} 1000"));
        assert!(p.contains("ghr_api_max_age_seconds 180"));
    }

    /// Unknown is not divergent, and stale is not divergent. An alert must never
    /// fire because we stopped being able to ask.
    #[test]
    fn a_stale_or_unknown_github_view_is_not_divergent() {
        // No api rows at all ⇒ Unknown.
        let p = Snapshot::gather(&seed(), 1100, "9.9.9", 180)
            .unwrap()
            .to_prometheus();
        assert!(!p.contains("ghr_runner_divergent{agent_id=\"1\""));

        // Rows exist but are far older than the window ⇒ Stale, still not
        // divergent, and the GitHub online series drops out rather than
        // asserting a stale value.
        let p = Snapshot::gather(&seed_divergent(), 100_000, "9.9.9", 180)
            .unwrap()
            .to_prometheus();
        assert!(p.contains("ghr_fleet_by_state{state=\"divergent\"} 0"));
        assert!(!p.contains("ghr_runner_github_online{agent_id=\"1\""));
    }

    #[test]
    fn prometheus_has_expected_families() {
        let snap = Snapshot::gather(&seed(), 1100, "9.9.9", 180).unwrap();
        let p = snap.to_prometheus();
        assert!(p.contains("ghr_build_info{version=\"9.9.9\"} 1"));
        assert!(p.contains("ghr_fleet_runners 2"));
        assert!(p.contains("ghr_fleet_by_state{state=\"busy\"} 1"));
        assert!(p.contains("ghr_host_load1 1.5"));
        assert!(p.contains("ghr_jobs_running 1"));
        // state_seconds = now(1100) - since_ts(900) = 200
        assert!(p.contains(
            "ghr_runner_state_seconds{agent_id=\"1\",name=\"r1\",org=\"acme\",state=\"busy\"} 200"
        ));
        assert!(p.contains("ghr_runner_cpu_percent{agent_id=\"1\",name=\"r1\",org=\"acme\"} 12.5"));
        // Working-set headline gauge and the raw cache-inclusive sibling.
        assert!(
            p.contains("ghr_runner_mem_bytes{agent_id=\"1\",name=\"r1\",org=\"acme\"} 1048576")
        );
        assert!(p.contains(
            "ghr_runner_mem_current_bytes{agent_id=\"1\",name=\"r1\",org=\"acme\"} 2097152"
        ));
    }

    #[test]
    fn json_has_fleet_plus_runner_records() {
        let snap = Snapshot::gather(&seed(), 1100, "9.9.9", 180).unwrap();
        let v: serde_json::Value = serde_json::from_str(&snap.to_json()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3); // 1 fleet + 2 runners
        assert!(
            arr.iter()
                .any(|o| o["kind"] == "fleet" && o["runners"] == 2)
        );
        assert!(arr.iter().any(|o| o["kind"] == "runner"
            && o["name"] == "r1"
            && o["busy"] == 1
            && o["mem_bytes"] == 1048576
            && o["mem_current_bytes"] == 2097152));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(esc(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
