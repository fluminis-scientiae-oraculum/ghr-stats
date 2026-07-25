//! `ghr-stats explain` — findings, not gauges.
//!
//! `status` answers *is the fleet healthy*. This answers *why isn't it*, in the
//! form the incident that motivated this work actually needed: a claim, and the
//! **boundary** to investigate. Establishing which side of the fence a fault sits
//! on was the single most expensive part of that investigation, and a fleet
//! monitor is uniquely placed to shortcut it, because it is the one process
//! holding both the local process truth and GitHub's opinion at the same instant.
//!
//! It is a pure derivation over the same [`FleetStatus`] snapshot `status`
//! reasons over — deliberately no new IPC query. A second query would be a second
//! chance for the two verbs to disagree, and "healthy" from one while the other
//! lists findings is a worse failure than either being wrong alone.

use anyhow::Result;
use serde::Serialize;

use crate::cli::ExplainArgs;
use crate::ops::status::{Snapshot, Source};
use crate::shared::config::Config;
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::models::{FleetStatus, Liveness, Mode, RunnerStatus, Verdict};
use crate::shared::util::{BUILD_VERSION, to_rfc3339_utc};

/// Which side of the fence to investigate — the load-bearing field of a finding.
///
/// An agent that knows *something is wrong* still has to guess where to look;
/// this is the guess the tool can make better than the caller. It is also the
/// shared vocabulary the other machine-facing verbs speak, so it is deliberately
/// small and closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Boundary {
    /// This host: the runner process, its unit, or its disk.
    Local,
    /// GitHub's side: the org's Actions service, its permissions, its shard.
    Github,
    /// Between the two: this host's egress, DNS, a proxy.
    Network,
    /// Our own configuration: a missing PAT, an org we were never told about.
    Config,
}

/// How much a finding should move the reader. Ranked by how *invisible* the
/// problem is elsewhere, not by how loud it sounds: divergence outranks a plainly
/// offline runner because every other surface already shows the offline one in
/// red, while divergence is precisely the failure that reads as green everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    High,
    Medium,
    /// Not a fault — a stated limit on what this answer could cover.
    Info,
}

/// One thing worth telling the caller.
///
/// `id` is `&'static str` rather than `String` because the set of findings is
/// closed at compile time: an id is a name in this file's vocabulary, not data
/// read from anywhere, so it cannot be typo'd into existence at runtime.
///
/// `claim` is the assertion, `evidence` is what it rests on, and
/// `suggested_checks` is what to do about it. Keeping them separate is the point:
/// an agent can act on the claim, a human can audit the evidence, and neither has
/// to parse a paragraph to find the other.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub boundary: Boundary,
    pub claim: String,
    /// The observations the claim rests on — always concrete counts, names or
    /// timestamps, never a restatement of the claim.
    pub evidence: Vec<String>,
    /// When the condition began, ISO-8601 UTC. `None` when the snapshot carries
    /// no duration to work back from; never guessed.
    pub first_seen: Option<String>,
    /// What to look at, in the order worth looking. Ordered by the `boundary`,
    /// since that is the whole reason the field exists.
    pub suggested_checks: Vec<String>,
}

/// The `explain` payload. Carries `mode` and `verdict` from the snapshot it was
/// derived from, so an empty `findings` array is never ambiguous: the reader can
/// see whether we had nothing to report or nothing to report *with*.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Explanation {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub mode: Mode,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
}

/// Run the verb. Returns the snapshot's verdict, so `explain` and `status` exit
/// with the same code for the same fleet.
pub fn run(args: &ExplainArgs, cfg: &Config) -> Result<Verdict> {
    let explanation = explain(&crate::ops::status::snapshot(cfg));

    if args.json {
        println!("{}", serde_json::to_string_pretty(&explanation)?);
    } else {
        print!("{}", human(&explanation));
    }
    Ok(explanation.verdict)
}

/// Derive the explanation from a snapshot. Pure — the whole verb is testable
/// without a socket, a database, or a fleet.
fn explain(snap: &Snapshot) -> Explanation {
    let s = &snap.status;
    Explanation {
        schema_version: 1,
        generated_at: s.generated_at.clone(),
        generated_at_epoch: s.generated_at_epoch,
        mode: s.mode,
        verdict: s.verdict,
        findings: findings(snap),
    }
}

/// Every finding this build can make, worst first — so a caller that reads only
/// the head of the list still reads the thing that matters most. Pure.
fn findings(snap: &Snapshot) -> Vec<Finding> {
    [
        divergence(&snap.status),
        offline_locally(&snap.status),
        github_view_stale(snap),
        github_view_unavailable(&snap.source),
        org_never_reconciled(snap),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Runners that are healthy locally while GitHub will not dispatch to them —
/// the failure this whole release exists to make visible.
fn divergence(s: &FleetStatus) -> Option<Finding> {
    let divergent: Vec<&RunnerStatus> = s
        .runners
        .iter()
        .filter(|r| r.divergent == Some(true))
        .collect();
    if divergent.is_empty() {
        return None;
    }

    let affected = orgs_of(divergent.iter().copied());
    let present = orgs_of(s.runners.iter());
    let boundary = divergence_boundary(affected.len(), present.len());
    let orgs = affected.join(", ");
    let claim = match boundary {
        Boundary::Network => format!(
            "{} runners are healthy locally but offline to GitHub, across EVERY org on this host \
             ({orgs}) — the shared factor is this host, not any one org.",
            divergent.len()
        ),
        // Only reached with peers to compare against, or with nothing to compare
        // against at all; the claim says which, because a one-org host cannot
        // distinguish "GitHub broke" from "our egress broke" and must not pretend to.
        _ if present.len() > 1 => format!(
            "{} runners in {orgs} are healthy locally but offline to GitHub, while every other \
             org on this host is online — the differing factor is the org.",
            divergent.len()
        ),
        _ => format!(
            "{} runners in {orgs} are healthy locally but offline to GitHub. This host serves \
             only that org, so there is no peer to rule the network out.",
            divergent.len()
        ),
    };

    // Work back from the LONGEST outage, not the newest: the earliest onset is
    // when the condition started, and the later ones are it spreading.
    let longest = divergent
        .iter()
        .filter_map(|r| r.github_offline_seconds)
        .max();
    let healthy: Vec<&str> = present
        .iter()
        .copied()
        .filter(|o| !affected.contains(o))
        .collect();

    let mut evidence = vec![format!(
        "{}/{} runners in {orgs} have a live local listener",
        divergent.len(),
        s.runners
            .iter()
            .filter(|r| affected.contains(&r.org.as_str()))
            .count()
    )];
    evidence.push(match longest {
        Some(secs) => format!(
            "github_online=false for {} runners, longest {secs}s",
            divergent.len()
        ),
        None => format!("github_online=false for {} runners", divergent.len()),
    });
    evidence.push(if healthy.is_empty() {
        "no peer org on this host is online to GitHub".to_string()
    } else {
        format!(
            "peer orgs on this host still online: {}",
            healthy.join(", ")
        )
    });

    Some(Finding {
        id: "github-divergence",
        severity: Severity::High,
        boundary,
        claim,
        evidence,
        first_seen: longest.map(|secs| to_rfc3339_utc(s.generated_at_epoch - secs)),
        suggested_checks: checks_for(boundary),
    })
}

/// What to look at, chosen by the boundary. The `network` list leads with this
/// host because that is what the boundary derivation just ruled *in*; the
/// `github` list leads with the provider for the same reason. Ordering the checks
/// by anything else would waste the one inference this verb exists to make.
fn checks_for(boundary: Boundary) -> Vec<String> {
    let checks: &[&str] = match boundary {
        Boundary::Network => &[
            "this host's egress to api.github.com (DNS, proxy, firewall, NAT)",
            "whether every org's PAT expired at the same time",
            "the provider status page, Actions component",
        ],
        _ => &[
            "the provider status page, Actions component",
            "compare the `serverUrl` shard in each runner's .runner across orgs",
            "confirm the org's Actions permissions and runner-group membership are unchanged",
            "confirm this org's PAT is unexpired and still has Self-hosted runners: Read",
        ],
    };
    checks.iter().map(|c| (*c).to_string()).collect()
}

/// Which side to investigate, from the *spread* of divergence across the orgs on
/// this host. This is the comparison the incident's human investigation had to
/// make by hand, and the only reason the tool can make it is that it sees every
/// org through one egress at one instant.
fn divergence_boundary(affected_orgs: usize, orgs_present: usize) -> Boundary {
    if affected_orgs == orgs_present && orgs_present > 1 {
        // Nothing org-specific survives as an explanation.
        Boundary::Network
    } else {
        // Peers are fine (or there are none) — the org is the variable.
        Boundary::Github
    }
}

/// Runners whose listener process is gone. Already visible in red on every other
/// surface, hence `Medium` — but `explain` would be lying by omission if the one
/// verb that claims to say *why* skipped the most ordinary cause.
fn offline_locally(s: &FleetStatus) -> Option<Finding> {
    let names = s
        .runners
        .iter()
        .filter(|r| r.liveness == Liveness::Offline)
        .map(|r| r.name.as_str())
        .collect::<Vec<_>>();
    if names.is_empty() {
        return None;
    }
    let longest = s
        .runners
        .iter()
        .filter(|r| r.liveness == Liveness::Offline)
        .map(|r| r.state_seconds)
        .max()
        .filter(|secs| *secs > 0);
    Some(Finding {
        id: "runners-offline-locally",
        severity: Severity::Medium,
        boundary: Boundary::Local,
        claim: format!(
            "{} runners have no listener process on this host: {}.",
            names.len(),
            names.join(", ")
        ),
        evidence: vec![
            format!("liveness=offline for {}/{}", names.len(), s.runners.len()),
            match longest {
                Some(secs) => format!("longest offline for {secs}s"),
                // Ephemeral has no persisted edge to measure from, so it reports
                // 0 — say nothing rather than report "offline for 0s".
                None => "no state duration available (no collector history)".to_string(),
            },
        ],
        first_seen: longest.map(|secs| to_rfc3339_utc(s.generated_at_epoch - secs)),
        suggested_checks: vec![
            "systemctl status for each runner's unit".to_string(),
            "the runner's own _diag logs under its install dir".to_string(),
            "disk space on the runner's work folder".to_string(),
        ],
    })
}

/// Runners the collector holds no CURRENT GitHub reading for, in an org that has
/// reconciled successfully at some point. It worked and stopped, which is a
/// different problem from never having been configured — see
/// [`org_never_reconciled`], which is the standing-condition half of this split.
///
/// Freshness is not re-adjudicated here: the reader already decided it once and
/// hands out `github_online: None` for a view that is stale or unknown. Re-testing
/// an age against a threshold in a second place is how the two would disagree.
fn github_view_stale(snap: &Snapshot) -> Option<Finding> {
    // Only the collector has a GitHub view at all; in a local scan every reading
    // is absent for one reason, already reported once by `github_view_unavailable`.
    if !matches!(snap.source, Source::Collector) {
        return None;
    }
    let s = &snap.status;
    let reconciled = |org: &str| {
        s.orgs
            .iter()
            .find(|o| o.org == org)
            .is_some_and(|o| o.reconcile_age_s.is_some())
    };
    let names: Vec<&str> = s
        .runners
        .iter()
        .filter(|r| r.github_online.is_none() && reconciled(&r.org))
        .map(|r| r.name.as_str())
        .collect();
    if names.is_empty() {
        return None;
    }
    let oldest = s
        .orgs
        .iter()
        .filter(|o| {
            s.runners
                .iter()
                .any(|r| r.org == o.org && r.github_online.is_none())
        })
        .filter_map(|o| o.reconcile_age_s)
        .max();
    Some(Finding {
        id: "github-view-stale",
        severity: Severity::Medium,
        boundary: Boundary::Github,
        claim: format!(
            "{} runners have no current GitHub reading even though their org has reconciled \
             before: {}. Their local state is known; GitHub's opinion of them is not.",
            names.len(),
            names.join(", ")
        ),
        evidence: vec![
            format!(
                "github_online=null for {}/{} runners",
                names.len(),
                s.runners.len()
            ),
            match oldest {
                Some(age) => format!("last successful reconcile for the affected orgs: {age}s ago"),
                None => "no successful reconcile timestamp for the affected orgs".to_string(),
            },
        ],
        first_seen: oldest.map(|age| to_rfc3339_utc(s.generated_at_epoch - age)),
        suggested_checks: vec![
            "ghr_api_reconcile_ok and ghr_api_reconcile_errors_total on the metrics endpoint"
                .to_string(),
            "whether the org's PAT expired or lost Self-hosted runners: Read".to_string(),
            "whether these runners were removed from the org on GitHub's side".to_string(),
        ],
    })
}

/// Orgs that have NEVER reconciled successfully. Deliberately `Info`: a host can
/// legitimately carry an org it holds no token for — a personal account, say —
/// and that is a standing configuration fact, not an incident. Ranking it with
/// the failures would make `explain` cry wolf on every single run, which is how
/// a findings list stops being read.
fn org_never_reconciled(snap: &Snapshot) -> Option<Finding> {
    if !matches!(snap.source, Source::Collector) {
        return None;
    }
    let s = &snap.status;
    let orgs: Vec<&str> = s
        .orgs
        .iter()
        .filter(|o| o.reconcile_age_s.is_none())
        .map(|o| o.org.as_str())
        .collect();
    if orgs.is_empty() {
        return None;
    }
    let runners: usize = s
        .runners
        .iter()
        .filter(|r| orgs.contains(&r.org.as_str()))
        .count();
    Some(Finding {
        id: "org-never-reconciled",
        severity: Severity::Info,
        boundary: Boundary::Config,
        claim: format!(
            "{} orgs have never reconciled with GitHub: {}. Their {runners} runners are reported \
             from local state only, and can never be found divergent.",
            orgs.len(),
            orgs.join(", ")
        ),
        evidence: vec![
            format!(
                "reconcile_age_s=null for {}/{} orgs",
                orgs.len(),
                s.orgs.len()
            ),
            format!("{runners} runners have no GitHub side to compare against"),
        ],
        // "Never" has no onset to report, and inventing the collector's start
        // time here would be a guess dressed as a measurement.
        first_seen: None,
        suggested_checks: vec![
            "whether a read-only PAT is configured for each of these orgs".to_string(),
            "ghr_api_org_configured on the metrics endpoint".to_string(),
            "that this is expected — a personal account cannot expose org runners".to_string(),
        ],
    })
}

/// State the limit rather than let an empty `findings` array read as all-clear.
///
/// Without a collector there is no GitHub view at all, so the highest-severity
/// finding above is not *absent* — it is unassessable. Reporting silence here is
/// the same all-green lie, one layer up.
///
/// The claim is built from the provenance rather than assumed, because the four
/// ways to end up without a collector have four different remedies, and three of
/// them are actively worsened by being told to install one that is already there.
fn github_view_unavailable(source: &Source) -> Option<Finding> {
    let checks: Vec<String> = match source {
        Source::Collector => Vec::new(),
        Source::LocalScan(EphemeralReason::NoCollector) => vec![
            "`ghr-stats systemd install --system` (or `--user`)".to_string(),
            "`systemctl status ghr-stats` in case it is installed but stopped".to_string(),
        ],
        Source::LocalScan(EphemeralReason::VersionDrift { .. }) => vec![
            "`systemctl restart ghr-stats` — the binary was upgraded, the service was not"
                .to_string(),
            "`ghr-stats --version` against the version the unit's ExecStart points at".to_string(),
        ],
        Source::LocalScan(EphemeralReason::Denied) => vec![
            "the socket's permissions and the unit's RuntimeDirectoryMode".to_string(),
            "re-running as root, or as a member of the `ghr-stats` group".to_string(),
        ],
        Source::LocalScan(EphemeralReason::Unusable | EphemeralReason::QueryFailed) => vec![
            "`journalctl -u ghr-stats` for the collector's own errors".to_string(),
            "whether another client is holding the collector's only connection".to_string(),
            "that the socket belongs to a live collector and is not stale".to_string(),
        ],
    };
    let (boundary, claim) = match source {
        Source::Collector => return None,
        Source::LocalScan(EphemeralReason::NoCollector) => (
            Boundary::Config,
            "No collector is running, so this is a local scan only and nothing GitHub-side \
             could be assessed. Install it with `ghr-stats systemd install`."
                .to_string(),
        ),
        Source::LocalScan(EphemeralReason::VersionDrift { server }) => (
            Boundary::Config,
            format!(
                "A collector IS running but speaks IPC v{server} while this binary speaks \
                 v{client} — an upgraded binary whose service was never restarted. Every \
                 GitHub-side answer is unavailable until `systemctl restart ghr-stats` \
                 (or `--user`) reloads it. The fleet itself is fine; this is a client/server \
                 mismatch.",
                client = crate::shared::ipc::VERSION
            ),
        ),
        Source::LocalScan(EphemeralReason::Denied) => (
            Boundary::Local,
            "A collector socket exists but this process may not connect to it. Check the \
             unit's RuntimeDirectoryMode and the socket's permissions, or re-run as root."
                .to_string(),
        ),
        Source::LocalScan(EphemeralReason::Unusable) => (
            Boundary::Local,
            "A collector socket accepted the connection but the handshake did not complete, \
             so something IS listening and installing another would not help. Read \
             `journalctl -u ghr-stats` and confirm the socket belongs to a live collector."
                .to_string(),
        ),
        Source::LocalScan(EphemeralReason::QueryFailed) => (
            Boundary::Local,
            "A collector answered the handshake but not the query, so it is running and \
             speaks this wire version — the fault is its own, usually the database. Read \
             `journalctl -u ghr-stats`."
                .to_string(),
        ),
    };
    Some(Finding {
        id: "github-view-unavailable",
        severity: Severity::Info,
        boundary,
        claim,
        evidence: vec![format!("ipc: {}", reason_word(source))],
        // The fallback is observed now; how long it has been true is not knowable
        // from a snapshot that could not reach the thing that would know.
        first_seen: None,
        suggested_checks: checks,
    })
}

/// A stable machine-readable token for why we fell back, so an agent can branch
/// on the cause without matching on prose that may be reworded.
fn reason_word(source: &Source) -> &'static str {
    match source {
        Source::Collector => "connected",
        Source::LocalScan(EphemeralReason::NoCollector) => "no-collector",
        Source::LocalScan(EphemeralReason::VersionDrift { .. }) => "version-drift",
        Source::LocalScan(EphemeralReason::Denied) => "denied",
        Source::LocalScan(EphemeralReason::Unusable) => "handshake-failed",
        Source::LocalScan(EphemeralReason::QueryFailed) => "query-failed",
    }
}

/// The orgs represented by a set of runners, deduplicated and ordered, so the
/// rendered claim is byte-stable across runs (a diffing agent must not see churn
/// that came from a hash order).
fn orgs_of<'a>(runners: impl Iterator<Item = &'a RunnerStatus>) -> Vec<&'a str> {
    let mut orgs: Vec<&str> = runners.map(|r| r.org.as_str()).collect();
    orgs.sort_unstable();
    orgs.dedup();
    orgs
}

/// Plain-text rendering. No colour and no box drawing, for the same reason
/// `status` has none: the only difference from `--json` should be the shape.
fn human(e: &Explanation) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ghr-stats {BUILD_VERSION}  ·  {}  ·  {}  ·  {}",
        e.mode.as_str(),
        e.generated_at,
        verdict_word(e.verdict)
    );
    if e.findings.is_empty() {
        let _ = writeln!(out, "no findings");
        return out;
    }
    for f in &e.findings {
        let since = f
            .first_seen
            .as_deref()
            .map(|t| format!(" · since {t}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "[{}] {} · investigate: {}{since}",
            severity_word(f.severity),
            f.id,
            boundary_word(f.boundary)
        );
        let _ = writeln!(out, "  {}", f.claim);
        for e in &f.evidence {
            let _ = writeln!(out, "    - {e}");
        }
        for (i, c) in f.suggested_checks.iter().enumerate() {
            let _ = writeln!(out, "    {}. {c}", i + 1);
        }
    }
    out
}

fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Ok => "ok",
        Verdict::Degraded => "degraded",
        Verdict::Unknown => "unknown",
    }
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Info => "info",
    }
}

fn boundary_word(b: Boundary) -> &'static str {
    match b {
        Boundary::Local => "local",
        Boundary::Github => "github",
        Boundary::Network => "network",
        Boundary::Config => "config",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::models::{FleetCounts, OrgStatus};

    fn runner(name: &str, org: &str, liveness: Liveness, divergent: Option<bool>) -> RunnerStatus {
        RunnerStatus {
            name: name.into(),
            org: org.into(),
            agent_id: 1,
            liveness,
            state_seconds: 0,
            github_online: divergent.map(|d| !d),
            github_busy: Some(false),
            github_offline_seconds: Some(600),
            github_sample_age_s: Some(5),
            divergent,
            cpu_percent: None,
            mem_bytes: None,
        }
    }

    /// A persistent snapshot — the collector answered.
    fn status(mode: Mode, runners: Vec<RunnerStatus>) -> Snapshot {
        with_source(mode, runners, Source::Collector)
    }

    /// A fallback snapshot, with the reason we fell back.
    fn fell_back(reason: EphemeralReason, runners: Vec<RunnerStatus>) -> Snapshot {
        with_source(Mode::Ephemeral, runners, Source::LocalScan(reason))
    }

    fn with_source(mode: Mode, runners: Vec<RunnerStatus>, source: Source) -> Snapshot {
        Snapshot {
            status: fleet(mode, runners),
            source,
        }
    }

    fn fleet(mode: Mode, runners: Vec<RunnerStatus>) -> FleetStatus {
        FleetStatus {
            schema_version: 1,
            generated_at: "2026-07-26T00:00:00Z".into(),
            generated_at_epoch: 0,
            mode,
            verdict: Verdict::Degraded,
            fleet: FleetCounts {
                runners: runners.len() as u32,
                busy: 0,
                idle: runners.len() as u32,
                offline: 0,
                divergent: runners.iter().filter(|r| r.divergent == Some(true)).count() as u32,
            },
            orgs: Vec::new(),
            runners,
        }
    }

    /// The incident's own shape: one org dark, peers on the same host fine. The
    /// peer comparison is what makes this `github` and not `network`.
    #[test]
    fn one_org_dark_while_peers_are_fine_points_at_github() {
        let s = status(
            Mode::Persistent,
            vec![
                runner("a0", "org-a", Liveness::Idle, Some(false)),
                runner("b0", "org-b", Liveness::Idle, Some(true)),
                runner("b1", "org-b", Liveness::Idle, Some(true)),
            ],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.id, "github-divergence");
        assert_eq!(f.boundary, Boundary::Github);
        assert_eq!(f.severity, Severity::High);
        assert!(f.claim.contains("2 runners in org-b"));
        assert!(f.claim.contains("the differing factor is the org"));
    }

    /// Every org on the host dark at once: no org-specific explanation survives,
    /// so the shared factor — this host's path to GitHub — is what to check.
    #[test]
    fn every_org_dark_at_once_points_at_the_network() {
        let s = status(
            Mode::Persistent,
            vec![
                runner("a0", "org-a", Liveness::Idle, Some(true)),
                runner("b0", "org-b", Liveness::Idle, Some(true)),
            ],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.boundary, Boundary::Network);
        assert!(f.claim.contains("org-a, org-b"));
    }

    /// A single-org host has no peer to compare against, and must say so rather
    /// than pass its guess off as the peer comparison it could not run.
    #[test]
    fn a_single_org_host_admits_it_cannot_rule_out_the_network() {
        let s = status(
            Mode::Persistent,
            vec![runner("b0", "org-b", Liveness::Idle, Some(true))],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.boundary, Boundary::Github);
        assert!(f.claim.contains("no peer to rule the network out"));
    }

    /// The boundary rule itself, at its edges.
    #[test]
    fn boundary_is_network_only_when_every_one_of_several_orgs_is_affected() {
        assert_eq!(divergence_boundary(3, 3), Boundary::Network);
        assert_eq!(divergence_boundary(2, 3), Boundary::Github);
        assert_eq!(divergence_boundary(1, 1), Boundary::Github);
    }

    /// An unknown GitHub view is not divergence — the same rule `status` applies
    /// to the verdict. `explain` must not page on our own ignorance either.
    #[test]
    fn an_unknown_github_view_yields_no_divergence_finding() {
        let s = status(
            Mode::Persistent,
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        assert!(findings(&s).is_empty());
        assert!(human(&explain(&s)).contains("no findings"));
    }

    /// Ephemeral mode cannot see GitHub at all, so silence would read as
    /// all-clear — the exact failure this release exists to prevent.
    #[test]
    fn ephemeral_mode_states_that_it_could_not_look() {
        let s = fell_back(
            EphemeralReason::NoCollector,
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.id, "github-view-unavailable");
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.boundary, Boundary::Config);
        assert!(f.claim.contains("systemd install"));
    }

    /// The reason a collector was unreachable decides the advice. Telling an
    /// operator to install a collector that is running — and only needs a
    /// restart, or a permission fixed — is worse than saying nothing: it sends
    /// them to fix the one thing that is not broken.
    #[test]
    fn a_reachable_but_unusable_collector_is_never_reported_as_absent() {
        for (reason, expect_boundary, expect_phrase) in [
            (
                EphemeralReason::VersionDrift { server: 8 },
                Boundary::Config,
                "systemctl restart ghr-stats",
            ),
            (
                EphemeralReason::Denied,
                Boundary::Local,
                "may not connect to it",
            ),
            (
                EphemeralReason::QueryFailed,
                Boundary::Local,
                "journalctl -u ghr-stats",
            ),
        ] {
            let s = fell_back(reason, vec![runner("a0", "org-a", Liveness::Idle, None)]);
            let f = &findings(&s)[0];
            assert_eq!(f.id, "github-view-unavailable");
            assert_eq!(f.boundary, expect_boundary, "for {reason:?}");
            assert!(
                f.claim.contains(expect_phrase),
                "for {reason:?}: {}",
                f.claim
            );
            assert!(
                !f.claim.contains("systemd install"),
                "advised installing a collector that is already running: {}",
                f.claim
            );
        }
    }

    /// The drift claim must quote BOTH versions — one of them alone does not
    /// tell the operator which side to move.
    #[test]
    fn the_version_drift_claim_names_both_wire_versions() {
        let s = fell_back(
            EphemeralReason::VersionDrift { server: 8 },
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        let claim = &findings(&s)[0].claim;
        assert!(claim.contains("v8"), "{claim}");
        assert!(
            claim.contains(&format!("v{}", crate::shared::ipc::VERSION)),
            "{claim}"
        );
    }

    #[test]
    fn a_locally_offline_runner_is_reported_at_the_local_boundary() {
        let s = status(
            Mode::Persistent,
            vec![
                runner("a0", "org-a", Liveness::Offline, Some(false)),
                runner("a1", "org-a", Liveness::Idle, Some(false)),
            ],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.id, "runners-offline-locally");
        assert_eq!(f.boundary, Boundary::Local);
        assert!(f.claim.contains("a0"));
        assert!(!f.claim.contains("a1"));
    }

    /// Findings are emitted worst-first, so a caller that reads only the head of
    /// the list still reads the thing that matters most.
    #[test]
    fn findings_are_ordered_worst_first() {
        let s = fell_back(
            EphemeralReason::NoCollector,
            vec![
                runner("a0", "org-a", Liveness::Offline, Some(false)),
                runner("b0", "org-b", Liveness::Idle, Some(true)),
            ],
        );
        let ids: Vec<&str> = findings(&s).iter().map(|f| f.id).collect();
        assert_eq!(
            ids,
            [
                "github-divergence",
                "runners-offline-locally",
                "github-view-unavailable"
            ]
        );
    }

    fn org(name: &str, reconcile_age_s: Option<i64>) -> OrgStatus {
        OrgStatus {
            org: name.into(),
            runners: 1,
            github_online: 0,
            reconcile_age_s,
            verdict: Verdict::Ok,
        }
    }

    fn with_orgs(runners: Vec<RunnerStatus>, orgs: Vec<OrgStatus>) -> Snapshot {
        let mut snap = status(Mode::Persistent, runners);
        snap.status.orgs = orgs;
        snap
    }

    /// The claim asserts; the evidence has to be checkable independently of it.
    /// Naming the healthy peers is the whole basis of the github-vs-network call,
    /// so a reader must be able to audit that call without rerunning the tool.
    #[test]
    fn the_divergence_finding_evidences_the_peer_comparison_it_relied_on() {
        let s = status(
            Mode::Persistent,
            vec![
                runner("a0", "org-a", Liveness::Idle, Some(false)),
                runner("b0", "org-b", Liveness::Idle, Some(true)),
            ],
        );
        let f = &findings(&s)[0];
        assert!(
            f.evidence.iter().any(|e| e.contains("org-a")),
            "evidence must name the healthy peer: {:?}",
            f.evidence
        );
        // github_offline_seconds is 600 in the fixture, generated_at_epoch is 0.
        assert_eq!(f.first_seen.as_deref(), Some("1969-12-31T23:50:00Z"));
        assert!(f.suggested_checks.iter().any(|c| c.contains("status page")));
    }

    /// The onset is the EARLIEST, so it must come from the longest outage. Taking
    /// the newest would report the moment the fault spread, not when it began.
    #[test]
    fn first_seen_comes_from_the_longest_outage_not_the_latest() {
        let mut recent = runner("b0", "org-b", Liveness::Idle, Some(true));
        recent.github_offline_seconds = Some(60);
        let mut old = runner("b1", "org-b", Liveness::Idle, Some(true));
        old.github_offline_seconds = Some(3600);
        let mut s = status(Mode::Persistent, vec![recent, old]);
        s.status.generated_at_epoch = 10_000;

        assert_eq!(
            findings(&s)[0].first_seen.as_deref(),
            Some(to_rfc3339_utc(10_000 - 3600).as_str())
        );
    }

    /// Never-configured is a standing fact, not an incident. This host carries an
    /// org it can never reconcile, so ranking it above Info would make `explain`
    /// cry wolf on every single run — which is how a findings list stops being read.
    #[test]
    fn a_never_reconciled_org_is_info_and_carries_no_onset() {
        let s = with_orgs(
            vec![runner("p0", "personal", Liveness::Idle, None)],
            vec![org("personal", None)],
        );
        let f = findings(&s)
            .into_iter()
            .find(|f| f.id == "org-never-reconciled")
            .expect("finding");
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.boundary, Boundary::Config);
        // "Never" has no onset; inventing the collector's start time would be a
        // guess dressed as a measurement.
        assert_eq!(f.first_seen, None);
        assert!(f.claim.contains("personal"));
    }

    /// "Worked and stopped" is a different problem from "never worked", and only
    /// the first is a change worth chasing. The split is what keeps the standing
    /// config gap out of the Medium band.
    #[test]
    fn a_stale_view_is_reported_only_for_an_org_that_has_reconciled_before() {
        let s = with_orgs(
            vec![
                runner("w0", "worked", Liveness::Idle, None),
                runner("n0", "never", Liveness::Idle, None),
            ],
            vec![org("worked", Some(900)), org("never", None)],
        );
        let ids: Vec<&str> = findings(&s).iter().map(|f| f.id).collect();
        assert_eq!(ids, ["github-view-stale", "org-never-reconciled"]);

        let stale = &findings(&s)[0];
        assert_eq!(stale.severity, Severity::Medium);
        assert_eq!(stale.boundary, Boundary::Github);
        assert!(stale.claim.contains("w0"), "{}", stale.claim);
        assert!(!stale.claim.contains("n0"), "{}", stale.claim);
        assert_eq!(stale.first_seen.as_deref(), Some("1969-12-31T23:45:00Z"));
    }

    /// Neither collector-only finding may fire from a local scan: without a
    /// collector there are no orgs and no adjudicated freshness, so both would be
    /// asserting things about data that was never fetched.
    #[test]
    fn collector_only_findings_stay_silent_in_a_local_scan() {
        let s = fell_back(
            EphemeralReason::NoCollector,
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        let ids: Vec<&str> = findings(&s).iter().map(|f| f.id).collect();
        assert_eq!(ids, ["github-view-unavailable"]);
    }

    /// The checks are ordered BY the boundary — that ordering is the payload, not
    /// decoration. A network verdict that opened with "check the provider status
    /// page" would discard the inference that produced the verdict.
    #[test]
    fn suggested_checks_lead_with_the_side_the_boundary_named() {
        assert!(checks_for(Boundary::Network)[0].contains("this host's egress"));
        assert!(checks_for(Boundary::Github)[0].contains("provider status page"));
    }

    /// Machine-stable output: the org list in a claim must not depend on hash
    /// order, or a diffing agent sees churn that is not a change.
    #[test]
    fn org_lists_are_sorted_and_deduplicated() {
        let rs = [
            runner("z", "org-z", Liveness::Idle, Some(true)),
            runner("a", "org-a", Liveness::Idle, Some(true)),
            runner("a2", "org-a", Liveness::Idle, Some(true)),
        ];
        assert_eq!(orgs_of(rs.iter()), ["org-a", "org-z"]);
    }
}
