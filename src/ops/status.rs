//! `ghr-stats status` — the machine-facing read verb.
//!
//! Every other read path is either a full-screen TUI or a metrics format aimed
//! at a time-series database. This is the one an agent (or a shell script, or a
//! CI step) can call: one invocation, a stable payload on stdout, and an exit
//! code that encodes the verdict so a caller can branch without parsing.
//!
//! Two modes, mirroring the TUI's: when a collector answers on its socket the
//! snapshot — verdict included — comes from it, so `status`, `/metrics` and the
//! push sink can never disagree. With no collector it falls back to a live local
//! scan and reports `mode: "ephemeral"` with every `github_*` field `null`;
//! unknown is stated, never invented.

use anyhow::Result;

use crate::cli::StatusArgs;
use crate::shared::config::Config;
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::ipc::{Query, Request, Response};
use crate::shared::models::{FleetCounts, FleetStatus, Liveness, Mode, RunnerStatus, Verdict};
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

/// Run the verb. Returns the verdict so the caller can turn it into an exit
/// code — the payload and the exit status therefore come from one value.
pub fn run(args: &StatusArgs, cfg: &Config) -> Result<Verdict> {
    let mut status = snapshot(cfg).status;

    filter(&mut status, args);
    // Filtering changes what the verdict is ABOUT, so recompute it over the
    // surviving rows — otherwise `--org healthy-org` could inherit a "degraded"
    // caused entirely by a different org.
    status.verdict = verdict_for(&status);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", human(&status));
    }
    Ok(status.verdict)
}

/// Where an answer came from — and, when it is the fallback, why.
///
/// `FleetStatus::mode` says *that* we fell back; only this says *why*, and the
/// difference is the difference between "install the collector" and "restart the
/// one you have". `connect_any` already computes the reason and the dashboard
/// already shows it; dropping it here is what let a verb advise installing a
/// collector that had been running the whole time.
pub(crate) enum Source {
    /// The collector answered — [`FleetStatus::mode`] is [`Mode::Persistent`].
    Collector,
    /// A live local scan, and what stopped us reaching a collector —
    /// [`FleetStatus::mode`] is [`Mode::Ephemeral`]. Pairing the reason with the
    /// fallback in one variant is what keeps "ephemeral for no stated reason"
    /// off the table.
    LocalScan(EphemeralReason),
}

/// A snapshot together with its provenance.
pub(crate) struct Snapshot {
    pub status: FleetStatus,
    pub source: Source,
}

/// The fleet snapshot every machine-facing verb reasons over: the collector's if
/// one answers on its socket, otherwise a live local scan.
///
/// Shared with [`crate::ops::explain`] on purpose, and the only constructor of a
/// [`Snapshot`]. Two verbs each opening their own connection with their own
/// fallback rule is how `status` and `explain` would come to disagree about the
/// mode, or about the numbers — and a disagreement between "is it healthy" and
/// "why isn't it" is worse than either being wrong.
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
            // It handshook but did not answer. Fall back rather than fail — a
            // local scan still answers the liveness half of the question — but
            // say so precisely: this collector is up, not absent.
            _ => local(EphemeralReason::QueryFailed),
        },
        Err(reason) => local(reason),
    }
}

/// Restrict the payload to the requested org / runner.
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

/// The health call over whatever rows remain. Empty is `Unknown`, not `Ok`: an
/// answer of "nothing matched" must not exit 0 as though all were well.
fn verdict_for(status: &FleetStatus) -> Verdict {
    if status.runners.is_empty() {
        Verdict::Unknown
    } else if status.fleet.divergent > 0 || status.fleet.offline > 0 {
        Verdict::Degraded
    } else {
        Verdict::Ok
    }
}

/// No collector: sample the fleet locally, exactly as the TUI's Ephemeral mode
/// does. Liveness, CPU and memory are all locally observable; the GitHub view is
/// not, so those fields stay `null`.
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
            // Without the collector's persisted edge there is no history to
            // measure from — report 0 rather than guess a duration.
            state_seconds: 0,
            github_online: None,
            github_busy: None,
            github_offline_seconds: None,
            github_sample_age_s: None,
            // Divergence needs GitHub's opinion. Not knowing is not "fine".
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

/// The human rendering. Deliberately plain — no colour, no box drawing — so it
/// stays readable when piped, and so the only difference from `--json` is shape.
pub(crate) fn human(s: &FleetStatus) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let verdict = match s.verdict {
        Verdict::Ok => "ok",
        Verdict::Degraded => "degraded",
        Verdict::Unknown => "unknown",
    };
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
    // Name the divergent runners explicitly: the whole point is that they look
    // fine locally, so a summary line alone would not tell you which to check.
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
    fn exit_codes_follow_the_verdict() {
        assert_eq!(Verdict::Ok.exit_code(), 0);
        assert_eq!(Verdict::Degraded.exit_code(), 1);
        assert_eq!(Verdict::Unknown.exit_code(), 2);
    }

    #[test]
    fn a_divergent_runner_makes_the_fleet_degraded() {
        let s = status(vec![
            runner("a", "o", Liveness::Idle, Some(false)),
            runner("b", "o", Liveness::Idle, Some(true)),
        ]);
        assert_eq!(s.verdict, Verdict::Degraded);
        assert_eq!(s.fleet.divergent, 1);
        // The human output must name WHICH runner — it looks healthy locally.
        assert!(human(&s).contains("! b (o) is idle locally but offline to GitHub"));
    }

    /// Filtering must not inherit another org's problem.
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

    /// "Nothing matched" is not health.
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

    /// An unknown GitHub view must not be read as divergence, and so must not
    /// degrade the verdict — an agent must not page on our own ignorance.
    #[test]
    fn an_unknown_github_view_does_not_degrade_the_verdict() {
        let s = status(vec![runner("a", "o", Liveness::Idle, None)]);
        assert_eq!(s.verdict, Verdict::Ok);
        assert_eq!(s.fleet.divergent, 0);
    }
}
