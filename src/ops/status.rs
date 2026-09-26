//! `ghr-stats status`. With a collector the snapshot and verdict come from it, so
//! `status`, `/metrics` and the push sink agree; without one, a local scan with
//! every `github_*` field `null`.

use anyhow::Result;

use crate::cli::StatusArgs;
use crate::shared::config::Config;
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::{FleetCounts, FleetStatus, Liveness, Mode, RunnerStatus, Verdict};
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

pub fn run(args: &StatusArgs, cfg: &Config) -> Result<Verdict> {
    let mut status = snapshot(cfg).status;

    filter(&mut status, args);
    // Recompute over the surviving rows so a filter cannot inherit another org's verdict.
    status.verdict = verdict_for(&status);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", human(&status));
    }
    Ok(status.verdict)
}

/// Where the snapshot came from and, for a fallback, why.
pub(crate) enum Source {
    /// The collector answered — [`FleetStatus::mode`] is [`Mode::Persistent`].
    Collector,
    /// [`FleetStatus::mode`] is [`Mode::Ephemeral`].
    LocalScan(EphemeralReason),
}

pub(crate) struct Snapshot {
    pub status: FleetStatus,
    pub source: Source,
}

/// The only constructor of [`Snapshot`], shared by every machine-facing verb so
/// they cannot disagree.
pub(crate) fn snapshot(cfg: &Config) -> Snapshot {
    let local = |reason| Snapshot {
        status: ephemeral_status(cfg),
        source: Source::LocalScan(reason),
    };
    match crate::shared::ipc::client::Client::connect_any() {
        Ok(mut client) => match client.request(&Request::Query(Query::FleetStatus)) {
            Ok(Response::FleetStatus(s)) => Snapshot {
                status: *s,
                source: Source::Collector,
            },
            // Handshook but did not answer: the collector is up, not absent.
            _ => local(EphemeralReason::QueryFailed),
        },
        Err(reason) => local(reason),
    }
}

fn filter(status: &mut FleetStatus, args: &StatusArgs) {
    if let Some(org) = &args.org {
        status.runners.retain(|r| &r.org == org);
        status.orgs.retain(|o| &o.org == org);
    }
    if let Some(name) = &args.runner {
        status.runners.retain(|r| &r.name == name);
        let orgs: Vec<String> = status.runners.iter().map(|r| r.org.clone()).collect();
        status.orgs.retain(|o| orgs.contains(&o.org));
    }
    status.fleet = counts(&status.runners);
}

fn counts(runners: &[RunnerStatus]) -> FleetCounts {
    FleetCounts {
        runners: runners.len() as u32,
        busy: runners
            .iter()
            .filter(|r| r.liveness == Liveness::Busy)
            .count() as u32,
        idle: runners
            .iter()
            .filter(|r| r.liveness == Liveness::Idle)
            .count() as u32,
        offline: runners
            .iter()
            .filter(|r| r.liveness == Liveness::Offline)
            .count() as u32,
        divergent: runners.iter().filter(|r| r.divergent == Some(true)).count() as u32,
    }
}

/// Empty is `Unknown`, not `Ok`: "nothing matched" must not exit 0.
fn verdict_for(status: &FleetStatus) -> Verdict {
    if status.runners.is_empty() {
        Verdict::Unknown
    } else if status.fleet.divergent > 0 || status.fleet.offline > 0 {
        Verdict::Degraded
    } else {
        Verdict::Ok
    }
}

fn ephemeral_status(cfg: &Config) -> FleetStatus {
    let now = now_epoch();
    let snap = crate::shared::collectors::collect_local(&cfg.runner_roots, now, false);
    let runners: Vec<RunnerStatus> = snap
        .runners
        .into_iter()
        .map(|p| RunnerStatus {
            name: p.info.name,
            org: p.info.org,
            agent_id: p.info.agent_id,
            liveness: p.liveness,
            // No persisted edge to measure from; consumers read 0 as unknown.
            state_seconds: 0,
            github_online: None,
            github_busy: None,
            github_offline_seconds: None,
            github_sample_age_s: None,
            divergent: None,
            cpu_percent: None,
            mem_bytes: p.mem_bytes,
        })
        .collect();

    let mut status = FleetStatus {
        schema_version: 1,
        generated_at: to_rfc3339_utc(now),
        generated_at_epoch: now,
        mode: Mode::Ephemeral,
        verdict: Verdict::Unknown,
        fleet: counts(&runners),
        orgs: Vec::new(),
        runners,
    };
    status.verdict = verdict_for(&status);
    status
}

pub(crate) fn human(s: &FleetStatus) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let verdict = s.verdict.as_str();
    let _ = writeln!(
        out,
        "ghr-stats {BUILD_VERSION}  ·  {}  ·  {}  ·  {verdict}",
        s.mode.as_str(),
        s.generated_at
    );
    let f = &s.fleet;
    let _ = writeln!(
        out,
        "{} runners · {} busy · {} idle · {} offline · {} GH-offline",
        f.runners, f.busy, f.idle, f.offline, f.divergent
    );
    for o in &s.orgs {
        let age = o
            .reconcile_age_s
            .map(|a| format!("{a}s ago"))
            .unwrap_or_else(|| "never".to_string());
        let _ = writeln!(
            out,
            "  {}: {}/{} online to GitHub · last reconcile {age}",
            o.org, o.github_online, o.runners
        );
    }
    for r in s.runners.iter().filter(|r| r.divergent == Some(true)) {
        let secs = r.github_offline_seconds.unwrap_or(0);
        let _ = writeln!(
            out,
            "  ! {} ({}) is {} locally but offline to GitHub for {}s",
            r.name,
            r.org,
            r.liveness.as_str(),
            secs
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(name: &str, org: &str, liveness: Liveness, divergent: Option<bool>) -> RunnerStatus {
        RunnerStatus {
            name: name.into(),
            org: org.into(),
            agent_id: 1,
            liveness,
            state_seconds: 0,
            github_online: divergent.map(|d| !d),
            github_busy: Some(false),
            github_offline_seconds: None,
            github_sample_age_s: Some(5),
            divergent,
            cpu_percent: None,
            mem_bytes: None,
        }
    }

    fn status(runners: Vec<RunnerStatus>) -> FleetStatus {
        let mut s = FleetStatus {
            schema_version: 1,
            generated_at: to_rfc3339_utc(0),
            generated_at_epoch: 0,
            mode: Mode::Persistent,
            verdict: Verdict::Unknown,
            fleet: counts(&runners),
            orgs: Vec::new(),
            runners,
        };
        s.verdict = verdict_for(&s);
        s
    }

    #[test]
    fn a_divergent_runner_makes_the_fleet_degraded() {
        let s = status(vec![
            runner("a", "o", Liveness::Idle, Some(false)),
            runner("b", "o", Liveness::Idle, Some(true)),
        ]);
        assert_eq!(s.verdict, Verdict::Degraded);
        assert_eq!(s.fleet.divergent, 1);
        assert!(human(&s).contains("! b (o) is idle locally but offline to GitHub"));
    }

    #[test]
    fn filtering_to_a_healthy_org_recomputes_the_verdict() {
        let mut s = status(vec![
            runner("a", "good", Liveness::Idle, Some(false)),
            runner("b", "bad", Liveness::Idle, Some(true)),
        ]);
        assert_eq!(s.verdict, Verdict::Degraded);

        let args = StatusArgs {
            json: false,
            org: Some("good".into()),
            runner: None,
        };
        filter(&mut s, &args);
        s.verdict = verdict_for(&s);
        assert_eq!(s.verdict, Verdict::Ok);
        assert_eq!(s.fleet.runners, 1);
    }

    #[test]
    fn an_empty_result_is_unknown_not_ok() {
        let mut s = status(vec![runner("a", "o", Liveness::Idle, Some(false))]);
        let args = StatusArgs {
            json: false,
            org: Some("nonexistent".into()),
            runner: None,
        };
        filter(&mut s, &args);
        s.verdict = verdict_for(&s);
        assert_eq!(s.verdict, Verdict::Unknown);
        assert_eq!(s.verdict.exit_code(), 2);
    }

    #[test]
    fn an_unknown_github_view_does_not_degrade_the_verdict() {
        let s = status(vec![runner("a", "o", Liveness::Idle, None)]);
        assert_eq!(s.verdict, Verdict::Ok);
        assert_eq!(s.fleet.divergent, 0);
    }
}
