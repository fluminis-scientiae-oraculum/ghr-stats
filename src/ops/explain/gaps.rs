//! Findings derived from what the snapshot LACKS.
//!
//! Three degrees of the same absence, and the whole point is that they are
//! distinguished rather than collapsed:
//!
//! - [`github_view_stale`] — the org reconciled before and has stopped. It
//!   worked, so something changed.
//! - [`org_never_reconciled`] — it never worked, which is a standing
//!   configuration fact and not an incident.
//! - [`github_view_unavailable`] — we could not reach the collector that holds
//!   the GitHub view at all, so nothing above could even be assessed.
//!
//! Reporting silence as all-clear is the failure this release exists to prevent,
//! so every one of these emits a finding rather than letting an empty list speak.
//! Severity is orthogonal to the seam: `github_view_stale` is `Medium` while the
//! other two are `Info`. What groups them is that the claim rests on missing
//! data, which is what constrains how it can be worded and what evidence can
//! back it. Findings about data we HAVE live in [`super::faults`].

use crate::ops::status::{Snapshot, Source};
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::util::to_rfc3339_utc;

use super::{Boundary, Finding, Severity};

/// Runners the collector holds no CURRENT GitHub reading for, in an org that has
/// reconciled successfully at some point. It worked and stopped, which is a
/// different problem from never having been configured — see
/// [`org_never_reconciled`], which is the standing-condition half of this split.
///
/// Freshness is not re-adjudicated here: the reader already decided it once and
/// hands out `github_online: None` for a view that is stale or unknown. Re-testing
/// an age against a threshold in a second place is how the two would disagree.
pub(super) fn github_view_stale(snap: &Snapshot) -> Option<Finding> {
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
pub(super) fn org_never_reconciled(snap: &Snapshot) -> Option<Finding> {
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
pub(super) fn github_view_unavailable(source: &Source) -> Option<Finding> {
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
        Source::LocalScan(reason) => reason.word(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::findings;
    use super::super::fixtures::{fell_back, runner, status};
    use super::*;
    use crate::shared::models::{Liveness, Mode, OrgStatus, RunnerStatus, Verdict};

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
}
