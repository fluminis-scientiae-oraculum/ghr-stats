//! Metric gathering + rendering. Reads the store on a caller-provided
//! connection, builds a [`Snapshot`], and renders it two ways: Prometheus text
//! exposition (for the pull endpoint) and a flat JSON array (for the push
//! sink). Pure reads + formatting — no DB writes, no I/O of its own.

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::error::Result;
use crate::shared::models::Liveness;

/// One runner's metric row.
struct RunnerMetric {
    agent_id: i64,
    name: String,
    org: String,
    liveness: Liveness,
    cpu_pct: Option<f32>,
    mem_bytes: Option<u64>,
    /// Seconds in the current liveness state (`now - since_ts`).
    state_seconds: i64,
    gh_online: Option<bool>,
    gh_busy: Option<bool>,
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
}

impl Snapshot {
    /// Read the current fleet state into a snapshot.
    pub fn gather(conn: &Connection, now: i64, version: &str) -> Result<Snapshot> {
        let latest = reader::latest_runners(conn)?;
        let states = reader::runner_states(conn)?;
        let api = reader::latest_api_runners(conn)?;
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
                let gh = api.get(&(r.org.clone(), r.agent_id));
                RunnerMetric {
                    agent_id: r.agent_id,
                    name: r.name,
                    org: r.org,
                    liveness: r.liveness,
                    cpu_pct: r.cpu_pct,
                    mem_bytes: r.mem_bytes,
                    state_seconds,
                    gh_online: gh.map(|s| s.online),
                    gh_busy: gh.map(|s| s.busy),
                }
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
             ghr_fleet_by_state{{state=\"offline\"}} {}",
            self.runners.len(),
            self.busy,
            self.idle,
            self.offline,
        );

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
            if let Some(o) = r.gh_online {
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
            if let Some(b) = r.gh_busy {
                let _ = writeln!(
                    s,
                    "ghr_runner_github_busy{{{}}} {}",
                    r.labels(),
                    i32::from(b)
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
                "state_seconds": r.state_seconds,
                "github_online": r.gh_online,
                "github_busy": r.gh_busy,
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
                "INSERT INTO runner_sample (ts,agent_id,name,org,liveness,cpu_pct,mem_bytes,dir) \
                 VALUES (1000,?1,?2,'acme',?3,12.5,1048576,?4)",
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

    #[test]
    fn prometheus_has_expected_families() {
        let snap = Snapshot::gather(&seed(), 1100, "9.9.9").unwrap();
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
    }

    #[test]
    fn json_has_fleet_plus_runner_records() {
        let snap = Snapshot::gather(&seed(), 1100, "9.9.9").unwrap();
        let v: serde_json::Value = serde_json::from_str(&snap.to_json()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3); // 1 fleet + 2 runners
        assert!(
            arr.iter()
                .any(|o| o["kind"] == "fleet" && o["runners"] == 2)
        );
        assert!(
            arr.iter()
                .any(|o| o["kind"] == "runner" && o["name"] == "r1" && o["busy"] == 1)
        );
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(esc(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
