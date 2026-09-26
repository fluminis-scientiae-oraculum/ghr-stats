//! Checks only the collector can answer; each starts by opening the socket.

use crate::ops::explain::Boundary;
use crate::shared::ipc::client::{Client, EphemeralReason};
use crate::shared::ipc::{self, Query, Request, Response};
use crate::shared::models::{FleetStatus, Verdict};
use crate::shared::util::{BUILD_VERSION, to_rfc3339_utc};

use super::{Check, Outcome, skipped};

pub(super) fn collector_checks() -> Vec<Check> {
    let mut client = match Client::connect_any() {
        Ok(c) => c,
        Err(reason) => {
            return vec![
                Check {
                    id: "collector",
                    boundary: Boundary::Local,
                    outcome: unreachable_outcome(&reason),
                },
                skipped("reconcile", Boundary::Github, "no collector answered"),
                skipped("history", Boundary::Local, "no collector answered"),
            ];
        }
    };

    let version = client.collector_version().unwrap_or("unknown").to_string();
    let mut checks = vec![Check {
        id: "collector",
        boundary: Boundary::Local,
        outcome: version_outcome(&version),
    }];

    match client.request(&Request::Query(Query::FleetStatus)) {
        Ok(Response::FleetStatus(s)) => checks.push(reconcile_check(&s)),
        // Handshook, then refused the query: a collector fault, not an absence.
        _ => checks.push(Check {
            id: "reconcile",
            boundary: Boundary::Local,
            outcome: Outcome::Fail {
                detail: "the collector is up but did not answer a status query".to_string(),
                fix: "check `journalctl -u ghr-stats -n 50` for a database error".to_string(),
            },
        }),
    }
    checks.push(history_check(&mut client));
    checks
}

fn unreachable_outcome(reason: &EphemeralReason) -> Outcome {
    match reason {
        EphemeralReason::VersionDrift { server } => Outcome::Fail {
            detail: format!(
                "the collector speaks wire v{server}, this binary speaks v{} — almost always a \
                 binary upgraded without restarting the service",
                ipc::VERSION
            ),
            fix: "sudo systemctl restart ghr-stats.service".to_string(),
        },
        other => Outcome::Fail {
            detail: match other.detail() {
                Some(why) => format!(
                    "no usable collector on the socket ({}): {why}",
                    other.word()
                ),
                None => format!("no usable collector on the socket ({})", other.word()),
            },
            fix: "check `systemctl status ghr-stats.service` and `journalctl -u ghr-stats -n 50`"
                .to_string(),
        },
    }
}

/// A wire mismatch is refused at handshake; this catches same wire, different build.
fn version_outcome(collector: &str) -> Outcome {
    if collector == BUILD_VERSION {
        Outcome::Pass {
            detail: format!("v{collector}, wire v{}", ipc::VERSION),
        }
    } else {
        Outcome::Fail {
            detail: format!(
                "the collector runs v{collector} but this binary is v{BUILD_VERSION} — they \
                 still share wire v{}, so nothing has broken yet",
                ipc::VERSION
            ),
            fix: "sudo systemctl restart ghr-stats.service".to_string(),
        }
    }
}

/// An org that never reconciled passes: it has no PAT, or its runners are enterprise-level.
fn reconcile_check(s: &FleetStatus) -> Check {
    let mut stale = Vec::new();
    let mut never = Vec::new();
    let mut ok = 0usize;
    for o in &s.orgs {
        match o.reconcile_age_s {
            None => never.push(o.org.clone()),
            Some(age) if o.verdict == Verdict::Ok => {
                let _ = age;
                ok += 1;
            }
            Some(age) => stale.push(format!("{} ({age}s ago)", o.org)),
        }
    }
    let outcome = if stale.is_empty() {
        let mut detail = format!("{ok} org(s) reconciling");
        if !never.is_empty() {
            detail.push_str(&format!(
                "; never reconciled: {} (no PAT, or enterprise-level runners)",
                never.join(", ")
            ));
        }
        Outcome::Pass { detail }
    } else {
        Outcome::Fail {
            detail: format!("stale reconcile: {}", stale.join(", ")),
            fix: "check the org's PAT with `sudo ghr-stats doctor`, then \
                  `journalctl -u ghr-stats | grep reconcile`"
                .to_string(),
        }
    };
    Check {
        id: "reconcile",
        boundary: Boundary::Github,
        outcome,
    }
}

/// Pruning is manual, so this reports rather than judges.
fn history_check(client: &mut Client) -> Check {
    let outcome = match client.request(&Request::Query(Query::Retention)) {
        Ok(Response::Retention { earliest_ts }) => Outcome::Pass {
            detail: match earliest_ts {
                Some(first) => format!(
                    "the record starts {} — pruning is manual (`ghr-stats db prune --days N`)",
                    to_rfc3339_utc(first)
                ),
                // Not a failure: a fresh install looks like this for its first seconds.
                None => "no samples retained yet — the collector has not written one".to_string(),
            },
        },
        _ => Outcome::Skipped {
            why: "the collector did not answer a retention query".to_string(),
        },
    };
    Check {
        id: "history",
        boundary: Boundary::Local,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_collector_on_a_different_build_fails_with_the_restart() {
        match version_outcome("0.0.1") {
            Outcome::Fail { detail, fix } => {
                assert!(
                    detail.contains("0.0.1") && detail.contains(BUILD_VERSION),
                    "{detail}"
                );
                assert!(fix.contains("systemctl restart"), "{fix}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert!(matches!(
            version_outcome(BUILD_VERSION),
            Outcome::Pass { .. }
        ));
    }

    #[test]
    fn an_org_that_never_reconciled_is_reported_not_failed() {
        use crate::shared::models::{FleetCounts, Mode, OrgStatus};
        let s = FleetStatus {
            schema_version: 1,
            generated_at: "now".to_string(),
            generated_at_epoch: 0,
            mode: Mode::Persistent,
            verdict: Verdict::Ok,
            fleet: FleetCounts {
                runners: 0,
                busy: 0,
                idle: 0,
                offline: 0,
                divergent: 0,
            },
            orgs: vec![
                OrgStatus {
                    org: "reconciling".to_string(),
                    runners: 1,
                    github_online: 1,
                    reconcile_age_s: Some(30),
                    verdict: Verdict::Ok,
                },
                OrgStatus {
                    org: "personal".to_string(),
                    runners: 1,
                    github_online: 0,
                    reconcile_age_s: None,
                    verdict: Verdict::Ok,
                },
            ],
            runners: Vec::new(),
        };
        match reconcile_check(&s).outcome {
            Outcome::Pass { detail } => {
                assert!(detail.contains("1 org(s) reconciling"), "{detail}");
                assert!(detail.contains("personal"), "{detail}");
            }
            other => panic!("expected a pass, got {other:?}"),
        }
    }

    #[test]
    fn an_unusable_collector_reports_the_error_that_explains_it() {
        let outcome = unreachable_outcome(&EphemeralReason::Unusable {
            detail: "unexpected handshake reply".to_string(),
        });
        match outcome {
            Outcome::Fail { detail, .. } => {
                assert!(detail.contains("handshake-failed"), "{detail}");
                assert!(detail.contains("unexpected handshake reply"), "{detail}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_reason_without_a_detail_renders_the_word_alone() {
        match unreachable_outcome(&EphemeralReason::NoCollector) {
            Outcome::Fail { detail, .. } => {
                assert_eq!(detail, "no usable collector on the socket (no-collector)");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn wire_drift_keeps_the_restart_as_its_fix() {
        match unreachable_outcome(&EphemeralReason::VersionDrift { server: 9 }) {
            Outcome::Fail { detail, fix } => {
                assert!(detail.contains("wire v9"), "{detail}");
                assert!(detail.contains(&format!("v{}", ipc::VERSION)), "{detail}");
                assert!(fix.contains("systemctl restart"), "{fix}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}
