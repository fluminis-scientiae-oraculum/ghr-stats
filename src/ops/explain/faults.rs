//! Findings derived from state we HAVE.
//!
//! Both findings in here read runner rows that are present in the snapshot and
//! assert something is wrong with them: the local listener is gone, or it is
//! alive while GitHub will not dispatch to it. The mirror image is [`super::gaps`],
//! which asserts things about rows or readings that are *missing*.
//!
//! The split is not by severity — [`super::gaps`] holds a `Medium` finding and
//! this file could hold an `Info` one. It is by whether the claim rests on data
//! the snapshot carries or on data it lacks, which is what decides how the claim
//! has to be worded and what evidence can back it.

use crate::shared::models::{FleetStatus, Liveness, RunnerStatus};
use crate::shared::util::to_rfc3339_utc;

use super::{Boundary, Finding, Severity};

/// Runners that are healthy locally while GitHub will not dispatch to them —
/// the failure this whole release exists to make visible.
pub(super) fn divergence(s: &FleetStatus) -> Option<Finding> {
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
pub(super) fn offline_locally(s: &FleetStatus) -> Option<Finding> {
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

/// The orgs represented by a set of runners, deduplicated and ordered, so the
/// rendered claim is byte-stable across runs (a diffing agent must not see churn
/// that came from a hash order).
fn orgs_of<'a>(runners: impl Iterator<Item = &'a RunnerStatus>) -> Vec<&'a str> {
    let mut orgs: Vec<&str> = runners.map(|r| r.org.as_str()).collect();
    orgs.sort_unstable();
    orgs.dedup();
    orgs
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{runner, status};
    use super::super::{explain, findings, human};
    use super::*;
    use crate::shared::models::{Liveness, Mode};

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
