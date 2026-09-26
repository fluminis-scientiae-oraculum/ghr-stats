//! Findings about what the snapshot lacks. Each emits a finding so that silence
//! never reads as all-clear.

use crate::ops::status::{Snapshot, Source};
use crate::shared::ipc::client::EphemeralReason;
use crate::shared::util::to_rfc3339_utc;

use super::{Boundary, Finding, Severity};

/// Runners with no current GitHub reading in an org that has reconciled before.
/// Freshness is already adjudicated upstream (`github_online: None`); no re-test here.
pub(super) fn github_view_stale(snap: &Snapshot) -> Option<Finding> {
    // A local scan's missing readings are reported once, by `github_view_unavailable`.
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

/// `Info`: an org with no token (e.g. a personal account) is a standing fact.
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
        first_seen: None,
        suggested_checks: vec![
            "whether a read-only PAT is configured for each of these orgs".to_string(),
            "ghr_api_org_configured on the metrics endpoint".to_string(),
            "that this is expected — a personal account cannot expose org runners".to_string(),
        ],
    })
}

/// Without a collector the GitHub-side findings are unassessable, not absent. Each
/// reason gets its own remedy: most must not be told to install a running collector.
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
        Source::LocalScan(EphemeralReason::Unusable { .. } | EphemeralReason::QueryFailed) => vec![
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
        Source::LocalScan(EphemeralReason::Unusable { .. }) => (
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
    // Error text goes in evidence only, never the claim or the branchable word.
    let mut evidence = vec![format!("ipc: {}", reason_word(source))];
    if let Some(why) = reason_detail(source) {
        evidence.push(format!("handshake error: {why}"));
    }
    Some(Finding {
        id: "github-view-unavailable",
        severity: Severity::Info,
        boundary,
        claim,
        evidence,
        first_seen: None,
        suggested_checks: checks,
    })
}

/// Stable token an agent can branch on.
fn reason_word(source: &Source) -> &'static str {
    match source {
        Source::Collector => "connected",
        Source::LocalScan(reason) => reason.word(),
    }
}

fn reason_detail(source: &Source) -> Option<&str> {
    match source {
        Source::Collector => None,
        Source::LocalScan(reason) => reason.detail(),
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
            let s = fell_back(
                reason.clone(),
                vec![runner("a0", "org-a", Liveness::Idle, None)],
            );
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

    #[test]
    fn an_unusable_collector_evidences_the_handshake_error() {
        let s = fell_back(
            EphemeralReason::Unusable {
                detail: "unexpected handshake reply".to_string(),
            },
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        let f = &findings(&s)[0];
        assert_eq!(f.id, "github-view-unavailable");
        assert!(
            f.evidence.iter().any(|e| e == "ipc: handshake-failed"),
            "{:?}",
            f.evidence
        );
        assert!(
            f.evidence
                .iter()
                .any(|e| e.contains("unexpected handshake reply")),
            "{:?}",
            f.evidence
        );
        assert!(
            !f.claim.contains("unexpected handshake reply"),
            "{}",
            f.claim
        );
    }

    #[test]
    fn a_reason_without_a_detail_adds_no_evidence_line() {
        let s = fell_back(
            EphemeralReason::NoCollector,
            vec![runner("a0", "org-a", Liveness::Idle, None)],
        );
        assert_eq!(findings(&s)[0].evidence, ["ipc: no-collector"]);
    }

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
        assert_eq!(f.first_seen, None);
        assert!(f.claim.contains("personal"));
    }

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
