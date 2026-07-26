//! `ghr-stats explain` — findings, not gauges.
//!
//! `status` answers *is the fleet healthy*. This answers *why isn't it*, in the
//! form the incident that motivated this work actually needed: a claim, and the
//! **boundary** to investigate. Establishing which side of the fence a fault sits
//! on was the single most expensive part of that investigation, and a fleet
//! monitor is uniquely placed to shortcut it, because it is the one process
//! holding both the local process truth and GitHub's opinion at the same instant.
//!
//! It is a pure derivation over the same
//! [`FleetStatus`](crate::shared::models::FleetStatus) snapshot `status`
//! reasons over — deliberately no new IPC query. A second query would be a second
//! chance for the two verbs to disagree, and "healthy" from one while the other
//! lists findings is a worse failure than either being wrong alone.
//!
//! # Layout
//!
//! The findings divide on what the claim RESTS ON: [`faults`] reads runner rows
//! the snapshot carries and asserts something is wrong with them; [`gaps`] asserts
//! things about rows and readings that are missing. That is the line that decides
//! how a claim can be worded and what evidence can back it, so it is the line the
//! files follow. Severity is orthogonal and would have been the wrong cut —
//! [`gaps`] holds a `Medium` finding alongside two `Info` ones.
//!
//! What stays here is the vocabulary every finding is written in, the ordering
//! that puts the worst first, and the plain-text rendering.

mod faults;
mod gaps;

use anyhow::Result;
use serde::Serialize;

use crate::cli::ExplainArgs;
use crate::ops::status::Snapshot;
use crate::shared::config::Config;
use crate::shared::models::{Mode, Verdict};
use crate::shared::util::BUILD_VERSION;

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
/// closed at compile time: an id is a name in this verb's vocabulary, not data
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
///
/// The order is written out here rather than derived from [`Severity`], because
/// it is a claim about which finding is most worth reading FIRST, not a sort key:
/// two findings can share a severity and still have a right order between them.
fn findings(snap: &Snapshot) -> Vec<Finding> {
    [
        faults::divergence(&snap.status),
        faults::offline_locally(&snap.status),
        gaps::github_view_stale(snap),
        gaps::github_view_unavailable(&snap.source),
        gaps::org_never_reconciled(snap),
    ]
    .into_iter()
    .flatten()
    .collect()
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

/// Snapshot builders shared by all three test modules.
///
/// They live here rather than in either child because a fixture duplicated per
/// file is a fixture that drifts per file, and these encode the shape the
/// findings are written against — `github_offline_seconds: Some(600)` against a
/// `generated_at_epoch` of 0 is what makes the `first_seen` assertions readable.
#[cfg(test)]
mod fixtures {
    use crate::ops::status::{Snapshot, Source};
    use crate::shared::ipc::client::EphemeralReason;
    use crate::shared::models::{FleetCounts, FleetStatus, Liveness, Mode, RunnerStatus, Verdict};

    pub(super) fn runner(
        name: &str,
        org: &str,
        liveness: Liveness,
        divergent: Option<bool>,
    ) -> RunnerStatus {
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
    pub(super) fn status(mode: Mode, runners: Vec<RunnerStatus>) -> Snapshot {
        with_source(mode, runners, Source::Collector)
    }

    /// A fallback snapshot, with the reason we fell back.
    pub(super) fn fell_back(reason: EphemeralReason, runners: Vec<RunnerStatus>) -> Snapshot {
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::{fell_back, runner};
    use super::*;
    use crate::shared::ipc::client::EphemeralReason;
    use crate::shared::models::Liveness;

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
}
