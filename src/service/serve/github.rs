//! The reconcile thread: what GitHub says, and how well we could ask.
//!
//! Runs on `api_secs`, the slow cadence, in its own thread precisely because it
//! is network-bound. Orgs come from the explicit `config.orgs` list when set, else
//! from a fresh scan of the runners' `.runner` files each cycle — so it shares no
//! mutable state with the local sampler.
//!
//! Degradation is per-ORG, never per-cycle: [`gather_api`] returns one
//! [`ApiOrgOutcome`] per org rather than a flat row list, because "GitHub says
//! this runner is offline", "we could not ask GitHub" and "no PAT is configured"
//! are three different facts. A flat `Vec<ApiRunnerRow>` collapsed them into one —
//! the failing org simply had no rows, so its series VANISHED instead of reporting
//! a value, which is how a four-hour outage looked like a healthy fleet.
//!
//! The same cycle opportunistically backfills finished jobs' pass/fail
//! `conclusion`, which the hook cannot know: the hook records TIMING and exits
//! while the job is still being graded. It is opportunistic because it needs the
//! token to also carry "Actions: read" — a runners-only token gets 403, which is
//! logged and skipped so the job simply keeps its neutral "done" state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::collectors::{self};
use crate::shared::config::{Config, SharedConfig};
use crate::shared::models::{ApiOrgOutcome, ApiRunnerRow, JobConclusion, PendingConclusion};
use crate::shared::util::now_epoch;

use super::{Sample, sleep_until};

/// Cap on how many pending job conclusions one reconcile cycle resolves — drains
/// a large backlog a batch at a time instead of a burst of API calls.
const JOB_RECONCILE_LIMIT: usize = 200;

/// Producer: reconcile GitHub's view on `api_secs`. Uses the explicit
/// `config.orgs` list when set, else discovers orgs from the runners' `.runner`
/// files each cycle — so it shares no mutable state with the local sampler. Each
/// cycle also resolves finished jobs' pass/fail `conclusion` from the Actions API
/// (opportunistic — see [`reconcile_job_conclusions`]), using its own reader.
pub(super) fn api_loop(
    cfg: &SharedConfig,
    term: &AtomicBool,
    tx: &Sender<Sample>,
    reader: Option<Connection>,
) {
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            // Snapshot per cycle: a PAT added via the TUI (AddOrgToken) is picked
            // up here on the next reconcile — no restart.
            let c = cfg.snapshot();
            let orgs: BTreeSet<String> = if c.orgs.is_empty() {
                collectors::runners::discover(&c.runner_roots)
                    .into_iter()
                    .map(|r| r.org)
                    .collect()
            } else {
                c.orgs.iter().cloned().collect()
            };
            let now = now_epoch();
            let outcomes = gather_api(&c, &orgs, term);
            // Send whenever an org was attempted, even if every one failed.
            // Gating on "produced rows" (the old behaviour) meant a total
            // reconcile failure left NO trace at all: no health row, no audit
            // row, and the previous tick's values kept being served as current.
            // A fleet-wide outage is exactly when the record matters most.
            if !outcomes.is_empty() && tx.send(Sample::Api { ts: now, outcomes }).is_err() {
                break; // writer gone
            }
            if let Some(conn) = reader.as_ref() {
                let updates = reconcile_job_conclusions(&c, conn, term);
                if !updates.is_empty() && tx.send(Sample::JobConclusions { updates }).is_err() {
                    break; // writer gone
                }
            }
            next = Instant::now() + Duration::from_secs(c.intervals.api_secs.max(10));
        }
        sleep_until(next, term);
    }
}

/// Query each org's runners (best-effort, per-org). A missing token, permission
/// error, or network failure degrades that org, never the cycle. Bails between
/// orgs if shutdown was signalled, so a SIGTERM mid-cycle exits promptly.
///
/// Returns one [`ApiOrgOutcome`] per org rather than a flat row list. The
/// distinction is load-bearing downstream: "GitHub says this runner is offline",
/// "we could not ask GitHub", and "no PAT is configured for this org" are three
/// different facts that a flat `Vec<ApiRunnerRow>` collapsed into one — the org
/// simply had no rows, so its series vanished instead of reporting a value.
fn gather_api(cfg: &Config, orgs: &BTreeSet<String>, term: &AtomicBool) -> Vec<ApiOrgOutcome> {
    let mut out = Vec::new();
    for org in orgs {
        if term.load(Ordering::SeqCst) {
            break;
        }
        let Some(token) = cfg.github_token_for(org) else {
            out.push(ApiOrgOutcome::Unconfigured { org: org.clone() });
            continue;
        };
        match crate::shared::github::list_org_runners_classified(&token, org) {
            Ok(runners) => out.push(ApiOrgOutcome::Ok {
                org: org.clone(),
                rows: runners
                    .into_iter()
                    .map(|r| ApiRunnerRow {
                        agent_id: r.id,
                        org: org.clone(),
                        name: r.name,
                        online: r.status == "online",
                        busy: r.busy,
                    })
                    .collect(),
            }),
            Err(kind) => {
                tracing::warn!(org = %org, kind = %kind.label(), hint = kind.hint(), "api reconcile failed");
                out.push(ApiOrgOutcome::Failed {
                    org: org.clone(),
                    kind,
                });
            }
        }
    }
    out
}

/// Resolve finished jobs' pass/fail `conclusion` from the Actions API. The hook
/// records job *timing*; this fills the conclusion. Opportunistic: it needs the
/// token to also carry "Actions: read" — a runners-only token gets 403, which we
/// log and skip so the job just keeps its neutral "done" state (no regression).
/// One `list_run_jobs` call per (repo, run) with pending rows; bails on shutdown.
fn reconcile_job_conclusions(
    cfg: &Config,
    conn: &Connection,
    term: &AtomicBool,
) -> Vec<JobConclusion> {
    let pending = reader::jobs_awaiting_conclusion(conn, JOB_RECONCILE_LIMIT).unwrap_or_default();
    if pending.is_empty() {
        return Vec::new();
    }
    // One API call per run: group the pending rows by (org, repo, run_id).
    let mut by_run: BTreeMap<(String, String, i64), Vec<PendingConclusion>> = BTreeMap::new();
    for p in pending {
        by_run
            .entry((p.org.clone(), p.repo.clone(), p.run_id))
            .or_default()
            .push(p);
    }
    let mut updates = Vec::new();
    for ((org, repo, run_id), jobs) in by_run {
        if term.load(Ordering::SeqCst) {
            break;
        }
        let Some(token) = cfg.github_token_for(&org) else {
            continue;
        };
        match crate::shared::github::list_run_jobs(&token, &repo, run_id) {
            Ok(api_jobs) => updates.extend(match_conclusions(&jobs, &api_jobs)),
            Err(e) => {
                tracing::debug!(error = %e, repo = %repo, run_id, "job-conclusion reconcile skipped")
            }
        }
    }
    updates
}

/// Match each pending job to its API job and collect the resolved conclusions.
/// A run with a single job maps regardless of name (covers a workflow `name:`
/// that differs from the job id the hook recorded); otherwise match by name. A
/// still-running job (conclusion `null`) is left for a later cycle. Pure.
fn match_conclusions(
    pending: &[PendingConclusion],
    api_jobs: &[crate::shared::github::RunJob],
) -> Vec<JobConclusion> {
    pending
        .iter()
        .filter_map(|p| {
            let concl = if api_jobs.len() == 1 {
                api_jobs[0].conclusion.clone()
            } else {
                api_jobs
                    .iter()
                    .find(|aj| aj.name == p.job)
                    .and_then(|aj| aj.conclusion.clone())
            };
            concl.map(|conclusion| JobConclusion {
                run_id: p.run_id,
                run_attempt: p.run_attempt,
                job: p.job.clone(),
                runner_name: p.runner_name.clone(),
                conclusion,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::github::RunJob;

    fn pending(job: &str) -> PendingConclusion {
        PendingConclusion {
            org: "example-org".into(),
            repo: "example-org/foo".into(),
            run_id: 1,
            run_attempt: 1,
            job: job.into(),
            runner_name: "runner-01".into(),
        }
    }
    fn api(name: &str, concl: Option<&str>) -> RunJob {
        RunJob {
            name: name.into(),
            conclusion: concl.map(str::to_string),
        }
    }

    #[test]
    fn match_conclusions_by_name_single_job_and_skips_running() {
        // Multi-job run: match by name; the still-running one (null) is skipped.
        let pend = [pending("build"), pending("test")];
        let jobs = [api("build", Some("success")), api("test", None)];
        let got = match_conclusions(&pend, &jobs);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].job.as_str(), got[0].conclusion.as_str()),
            ("build", "success")
        );

        // Single-job run: mapped regardless of name (a custom workflow `name:`).
        let got = match_conclusions(
            &[pending("deploy")],
            &[api("Deploy to prod", Some("failure"))],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].conclusion, "failure");

        // No matching API job in a multi-job run → nothing resolved.
        assert!(
            match_conclusions(
                &[pending("nope")],
                &[api("a", Some("success")), api("b", Some("success"))]
            )
            .is_empty()
        );
    }
}
