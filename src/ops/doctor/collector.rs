//! The checks only the collector can answer.
//!
//! Every check in here starts by opening the socket, and none of them can be
//! answered any other way: whether a daemon is there at all, which build it is,
//! what it last heard from GitHub, and how far back its record reaches. That
//! dependency is why they are together — a wire change lands in this file and
//! nowhere else in the verb.
//!
//! The mirror image lives in [`super::host`], which asks this machine directly
//! and never opens a socket. The two files share nothing but [`Check`] and
//! [`Outcome`]; their import lists do not intersect.

use crate::ops::explain::Boundary;
use crate::shared::ipc::client::Client;
use crate::shared::ipc::{self, Query, Request, Response};
use crate::shared::models::timeline::TimelineQuery;
use crate::shared::models::{FleetStatus, Verdict};
use crate::shared::util::{BUILD_VERSION, now_epoch, to_rfc3339_utc};

use super::{Check, Outcome, skipped};

/// How far back `doctor` looks when asking where the record starts. Only the
/// window's `truncated_at` is read, so the limit is 1: this asks a question
/// about retention, not for history.
const HISTORY_PROBE_SECS: i64 = 7 * 86_400;

/// Everything the collector can answer for: that it is there, that it speaks our
/// wire version, and — if it does — what it knows about reconciles and history.
pub(super) fn collector_checks() -> Vec<Check> {
    let mut client = match Client::connect_any() {
        Ok(c) => c,
        Err(reason) => {
            let (detail, fix) = match reason {
                crate::shared::ipc::client::EphemeralReason::VersionDrift { server } => (
                    format!(
                        "the collector speaks wire v{server}, this binary speaks v{} — almost \
                         always a binary upgraded without restarting the service",
                        ipc::VERSION
                    ),
                    "sudo systemctl restart ghr-stats.service".to_string(),
                ),
                other => (
                    format!("no usable collector on the socket ({})", other.word()),
                    "check `systemctl status ghr-stats.service` and \
                     `journalctl -u ghr-stats -n 50`"
                        .to_string(),
                ),
            };
            return vec![
                Check {
                    id: "collector",
                    boundary: Boundary::Local,
                    outcome: Outcome::Fail { detail, fix },
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
        // It handshook and then refused the query — its own fault, not an
        // absence, and the distinction is what tells "install one" from
        // "look at the one you have".
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

/// Binary semver against the collector's. Pure.
///
/// A wire mismatch never reaches here — the handshake refuses first — so this
/// catches the quieter case: same wire version, different build. That is a
/// restart that has not happened yet, and it is worth naming before it becomes a
/// wire break at the next bump.
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

/// Per-org reconcile freshness, from the snapshot the collector already
/// assembles. Pure.
///
/// An org that has NEVER reconciled is reported, not failed: this host has one
/// that never can (a personal account exposes no org runner API), and a check
/// that cries wolf on every run is a check the operator learns to skip. An org
/// that reconciled once and then stopped is the real fault.
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
                "; never reconciled: {} (expected for an account with no org runner API)",
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

/// Where the retained record starts.
///
/// Asked through the existing `timeline` query rather than a new one: its window
/// already reports `truncated_at` — where data begins when it begins after the
/// window opens — which is exactly the retention question. Pruning is manual, so
/// this reports rather than judges.
fn history_check(client: &mut Client) -> Check {
    let query = TimelineQuery {
        since_ts: now_epoch() - HISTORY_PROBE_SECS,
        limit: 1,
        org: None,
        runner: None,
        samples: false,
    };
    let outcome = match client.request(&Request::Query(Query::Timeline(query))) {
        Ok(Response::Timeline(t)) => Outcome::Pass {
            detail: match t.window.truncated_at {
                Some(first) => format!(
                    "the record starts {} — pruning is manual (`ghr-stats db prune --days N`)",
                    to_rfc3339_utc(first)
                ),
                None => format!(
                    "the record covers the full {}d probe — pruning is manual \
                     (`ghr-stats db prune --days N`)",
                    HISTORY_PROBE_SECS / 86_400
                ),
            },
        },
        _ => Outcome::Skipped {
            why: "the collector did not answer a history query".to_string(),
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
    use std::path::PathBuf;

    use super::super::host::{ConfigSource, config_check};
    use super::*;

    /// The drift that is live on this host right now, in its quieter form: same
    /// wire version, different build. Naming it before the next wire bump is the
    /// point — after the bump it stops being a warning and starts being a
    /// refused handshake.
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

    /// An org that has never reconciled is reported, not failed: this host has
    /// one that never can. A check that cries wolf every run is a check the
    /// operator learns to skip.
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

    /// Every failure ships an action. The type makes a fix mandatory; this
    /// pins that no arm slipped an empty string past it. It samples both halves
    /// of the verb deliberately — the invariant belongs to [`Outcome`], not to
    /// either file that constructs one.
    #[test]
    fn every_failure_carries_a_non_empty_fix() {
        let checks = vec![
            config_check(
                &ConfigSource::Missing {
                    path: PathBuf::from("/etc/ghr-stats/config.toml"),
                },
                &[],
            ),
            Check {
                id: "collector",
                boundary: Boundary::Local,
                outcome: version_outcome("0.0.1"),
            },
        ];
        for c in &checks {
            if let Outcome::Fail { fix, .. } = &c.outcome {
                assert!(!fix.trim().is_empty(), "{} has an empty fix", c.id);
            }
        }
    }
}
