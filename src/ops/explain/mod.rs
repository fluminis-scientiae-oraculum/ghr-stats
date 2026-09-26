//! `ghr-stats explain`: derived from the same snapshot `status` reasons over, with
//! no IPC query of its own, so the two verbs cannot disagree. [`faults`] makes
//! claims about runner rows present; [`gaps`] about rows and readings missing.

mod faults;
mod gaps;

use anyhow::Result;
use serde::Serialize;

use crate::cli::ExplainArgs;
use crate::ops::status::Snapshot;
use crate::shared::config::Config;
use crate::shared::models::{Mode, Verdict};
use crate::shared::util::BUILD_VERSION;

/// Which side of the fence to investigate.
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

/// Ranked by how invisible the problem is elsewhere: divergence outranks offline
/// because every other surface already shows an offline runner in red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Severity {
    High,
    Medium,
    /// Not a fault — a stated limit on what this answer could cover.
    Info,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Info => "info",
        }
    }
}

impl Boundary {
    fn as_str(self) -> &'static str {
        match self {
            Boundary::Local => "local",
            Boundary::Github => "github",
            Boundary::Network => "network",
            Boundary::Config => "config",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub boundary: Boundary,
    pub claim: String,
    /// Concrete counts, names or timestamps; never a restatement of the claim.
    pub evidence: Vec<String>,
    /// ISO-8601 UTC. `None` when the snapshot has no duration to work back from.
    pub first_seen: Option<String>,
    pub suggested_checks: Vec<String>,
}

/// Carries `mode` so an empty `findings` distinguishes "nothing to report" from
/// "nothing to report with".
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Explanation {
    pub schema_version: u32,
    pub generated_at: String,
    pub generated_at_epoch: i64,
    pub mode: Mode,
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
}

/// Returns the snapshot's verdict, so `explain` exits like `status`.
pub fn run(args: &ExplainArgs, cfg: &Config) -> Result<Verdict> {
    let explanation = explain(&crate::ops::status::snapshot(cfg));

    if args.json {
        crate::ops::emit_json(&explanation)?;
    } else {
        crate::ops::emit(&human(&explanation))?;
    }
    Ok(explanation.verdict)
}

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

/// Worst first. Hand-ordered rather than sorted by [`Severity`]: findings of
/// equal severity still have a right order.
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

fn human(e: &Explanation) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "ghr-stats {BUILD_VERSION}  ·  {}  ·  {}  ·  {}",
        e.mode.as_str(),
        e.generated_at,
        e.verdict.as_str()
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
            f.severity.as_str(),
            f.id,
            f.boundary.as_str()
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

/// Snapshot builders shared by the explain test modules.
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

    pub(super) fn status(mode: Mode, runners: Vec<RunnerStatus>) -> Snapshot {
        with_source(mode, runners, Source::Collector)
    }

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
