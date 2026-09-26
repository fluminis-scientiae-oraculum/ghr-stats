//! GitHub reconcile thread. Also backfills finished jobs' `conclusion`, which the hook
//! exits too early to see; that needs "Actions: read", so a runners-only token's 403 is
//! skipped.

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

/// Per cycle, so a large backlog drains in batches rather than an API burst.
const JOB_RECONCILE_LIMIT: usize = 200;

pub(super) fn api_loop(
    cfg: &SharedConfig,
    term: &AtomicBool,
    tx: &Sender<Sample>,
    reader: Option<Connection>,
) {
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            // Per cycle, so a PAT added over IPC applies without a restart.
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
            // Send even if every org failed, so a total outage still records health rows.
            if !outcomes.is_empty() && tx.send(Sample::Api { ts: now, outcomes }).is_err() {
                break;
            }
            if let Some(conn) = reader.as_ref() {
                let updates = reconcile_job_conclusions(&c, conn, term);
                if !updates.is_empty() && tx.send(Sample::JobConclusions { updates }).is_err() {
                    break;
                }
            }
            next = Instant::now() + Duration::from_secs(c.intervals.api_secs.max(10));
        }
        sleep_until(next, term);
    }
}

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

fn reconcile_job_conclusions(
    cfg: &Config,
    conn: &Connection,
    term: &AtomicBool,
) -> Vec<JobConclusion> {
    let pending = reader::jobs_awaiting_conclusion(conn, JOB_RECONCILE_LIMIT).unwrap_or_default();
    if pending.is_empty() {
        return Vec::new();
    }
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

/// A single-job run maps regardless of name: its workflow `name:` may differ from the job
/// id the hook recorded.
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
        let pend = [pending("build"), pending("test")];
        let jobs = [api("build", Some("success")), api("test", None)];
        let got = match_conclusions(&pend, &jobs);
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].job.as_str(), got[0].conclusion.as_str()),
            ("build", "success")
        );

        let got = match_conclusions(
            &[pending("deploy")],
            &[api("Deploy to prod", Some("failure"))],
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].conclusion, "failure");

        assert!(
            match_conclusions(
                &[pending("nope")],
                &[api("a", Some("success")), api("b", Some("success"))]
            )
            .is_empty()
        );
    }
}
