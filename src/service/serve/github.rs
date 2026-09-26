//! GitHub reconcile thread: what GitHub says about each registered runner, and the
//! conclusions of jobs the hooks saw finish (the hook exits before GitHub knows).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::collectors;
use crate::shared::config::{Config, SharedConfig};
use crate::shared::github::{self, GitHubHost, Owner, RunJob, RunnerScope};
use crate::shared::models::{
    ApiErrorKind, ApiOrgOutcome, ApiRunnerRow, JobConclusion, PendingConclusion, RunnerInfo,
};
use crate::shared::util::now_epoch;

use super::{Sample, sleep_until};

/// Conclusions are looked up for a day after a job finishes, then given up on.
const CONCLUSION_WINDOW_S: i64 = 86_400;
const PENDING_LIMIT: usize = 500;
/// Run attempts looked up per cycle, oldest first.
const RUN_LOOKUPS_PER_CYCLE: usize = 30;

pub(super) fn api_loop(
    cfg: &SharedConfig,
    term: &AtomicBool,
    tx: &Sender<Sample>,
    reader: Option<Connection>,
) {
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let c = cfg.snapshot();
            let discovered = collectors::runners::discover(&c.runner_roots);
            let now = now_epoch();
            let outcomes = gather_api(&c, &targets(&c, &discovered), term);
            if !outcomes.is_empty() && tx.send(Sample::Api { ts: now, outcomes }).is_err() {
                break;
            }
            if let Some(conn) = reader.as_ref() {
                let updates = reconcile_job_conclusions(&c, conn, &discovered, now, term);
                if !updates.is_empty() && tx.send(Sample::JobConclusions { updates }).is_err() {
                    break;
                }
            }
            next = Instant::now() + Duration::from_secs(c.intervals.api_secs.max(10));
        }
        sleep_until(next, term);
    }
}

type Targets = BTreeMap<(GitHubHost, String), BTreeSet<RunnerScope>>;

/// The scopes to list, grouped by `(host, login)`: those the runners here are
/// registered to, or the configured `orgs` on github.com.
fn targets(cfg: &Config, discovered: &[RunnerInfo]) -> Targets {
    let mut t = Targets::new();
    if cfg.orgs.is_empty() {
        for r in discovered {
            t.entry((r.scope.host.clone(), r.org.clone()))
                .or_default()
                .insert(r.scope.clone());
        }
    } else {
        for org in &cfg.orgs {
            let scope = RunnerScope {
                host: GitHubHost::dotcom(),
                owner: Owner::Org(org.clone()),
            };
            t.entry((GitHubHost::dotcom(), org.clone()))
                .or_default()
                .insert(scope);
        }
    }
    t
}

fn gather_api(cfg: &Config, targets: &Targets, term: &AtomicBool) -> Vec<ApiOrgOutcome> {
    let mut out = Vec::new();
    for ((host, org), scopes) in targets {
        if term.load(Ordering::SeqCst) {
            break;
        }
        let listable: Vec<&RunnerScope> =
            scopes.iter().filter(|s| s.runners_path().is_ok()).collect();
        let token = cfg.github_token_for(host, org);
        let (Some(token), false) = (token, listable.is_empty()) else {
            out.push(ApiOrgOutcome::Unconfigured { org: org.clone() });
            continue;
        };
        let mut rows = Vec::new();
        let mut failed = None;
        for scope in listable {
            match github::runners(scope, &token) {
                Ok(runners) => rows.extend(runners.into_iter().map(|r| ApiRunnerRow {
                    agent_id: r.id,
                    org: org.clone(),
                    name: r.name,
                    online: r.status == "online",
                    busy: r.busy,
                })),
                Err(kind) => {
                    failed = Some(kind);
                    break;
                }
            }
        }
        match failed {
            None => out.push(ApiOrgOutcome::Ok {
                org: org.clone(),
                rows,
            }),
            Some(kind) => {
                tracing::warn!(org = %org, host = %host, kind = %kind.label(), hint = kind.hint(), "api reconcile failed");
                out.push(ApiOrgOutcome::Failed {
                    org: org.clone(),
                    kind,
                });
            }
        }
    }
    out
}

/// Fill the conclusion of jobs that finished in the last day, looking up at most
/// [`RUN_LOOKUPS_PER_CYCLE`] run attempts, oldest first. An org whose token cannot
/// read Actions is skipped for the rest of the cycle.
fn reconcile_job_conclusions(
    cfg: &Config,
    conn: &Connection,
    discovered: &[RunnerInfo],
    now: i64,
    term: &AtomicBool,
) -> Vec<JobConclusion> {
    let pending = reader::jobs_awaiting_conclusion(conn, now - CONCLUSION_WINDOW_S, PENDING_LIMIT)
        .unwrap_or_default();
    let hosts: HashMap<(&str, &str), &GitHubHost> = discovered
        .iter()
        .map(|r| ((r.org.as_str(), r.name.as_str()), &r.scope.host))
        .collect();

    let mut attempts: Vec<Vec<PendingConclusion>> = Vec::new();
    let mut index: HashMap<(String, String, i64, i64), usize> = HashMap::new();
    for p in pending {
        let key = (p.org.clone(), p.repo.clone(), p.run_id, p.run_attempt);
        let i = *index.entry(key).or_insert_with(|| {
            attempts.push(Vec::new());
            attempts.len() - 1
        });
        attempts[i].push(p);
    }

    let dotcom = GitHubHost::dotcom();
    let mut denied: HashSet<String> = HashSet::new();
    let mut updates = Vec::new();
    for jobs in attempts.iter().take(RUN_LOOKUPS_PER_CYCLE) {
        if term.load(Ordering::SeqCst) {
            break;
        }
        let first = &jobs[0];
        if denied.contains(&first.org) || !github::is_repo_of(&first.repo, &first.org) {
            continue;
        }
        let host = hosts
            .get(&(first.org.as_str(), first.runner_name.as_str()))
            .copied()
            .unwrap_or(&dotcom);
        let Some(token) = cfg.github_token_for(host, &first.org) else {
            continue;
        };
        match github::attempt_jobs(host, &token, &first.repo, first.run_id, first.run_attempt) {
            Ok(api_jobs) => updates.extend(match_conclusions(jobs, &api_jobs)),
            Err(ApiErrorKind::Forbidden) => {
                denied.insert(first.org.clone());
            }
            Err(kind) => tracing::debug!(
                repo = %first.repo, run_id = first.run_id, kind = %kind.label(),
                "job conclusion lookup failed"
            ),
        }
    }
    updates
}

/// Match hook-recorded jobs to the attempt's API jobs. The hook knows the job id
/// (`GITHUB_JOB`), the API a display name, so a job is matched on the runner that ran
/// it, then on a name equal to the id, a single matrix leg `<id> (…)`, or being that
/// runner's only job in the attempt. Anything ambiguous stays unresolved.
fn match_conclusions(pending: &[PendingConclusion], api_jobs: &[RunJob]) -> Vec<JobConclusion> {
    pending
        .iter()
        .filter_map(|p| {
            let on_runner: Vec<&RunJob> = api_jobs
                .iter()
                .filter(|j| j.runner_name.as_deref() == Some(p.runner_name.as_str()))
                .collect();
            let leg = format!("{} (", p.job);
            let legs: Vec<&&RunJob> = on_runner
                .iter()
                .filter(|j| j.name.starts_with(&leg))
                .collect();
            let job = on_runner
                .iter()
                .find(|j| j.name == p.job)
                .or(match legs.as_slice() {
                    [one] => Some(*one),
                    _ => None,
                })
                .or(match on_runner.as_slice() {
                    [only] => Some(only),
                    _ => None,
                })?;
            Some(JobConclusion {
                run_id: p.run_id,
                run_attempt: p.run_attempt,
                job: p.job.clone(),
                runner_name: p.runner_name.clone(),
                conclusion: job.conclusion.clone()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(job: &str, runner: &str) -> PendingConclusion {
        PendingConclusion {
            org: "example-org".into(),
            repo: "example-org/foo".into(),
            run_id: 1,
            run_attempt: 1,
            job: job.into(),
            runner_name: runner.into(),
        }
    }

    fn api(name: &str, runner: Option<&str>, concl: Option<&str>) -> RunJob {
        RunJob {
            name: name.into(),
            runner_name: runner.map(str::to_string),
            conclusion: concl.map(str::to_string),
        }
    }

    #[test]
    fn jobs_match_by_runner_then_name() {
        let jobs = [
            api("build", Some("r1"), Some("success")),
            api("Test suite", Some("r2"), Some("failure")),
            api("lint (a)", Some("r3"), Some("success")),
            api("lint (b)", Some("r3"), Some("failure")),
            api("fmt (x)", Some("r5"), Some("success")),
            api("docs", Some("r5"), Some("success")),
            api("deploy", Some("r4"), None),
        ];
        let got = match_conclusions(
            &[
                pending("build", "r1"),
                pending("test", "r2"),
                pending("lint", "r3"),
                pending("fmt", "r5"),
                pending("deploy", "r4"),
                pending("build", "r9"),
            ],
            &jobs,
        );
        let pairs: Vec<(&str, &str, &str)> = got
            .iter()
            .map(|c| {
                (
                    c.job.as_str(),
                    c.runner_name.as_str(),
                    c.conclusion.as_str(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                ("build", "r1", "success"),
                ("test", "r2", "failure"),
                ("fmt", "r5", "success"),
            ]
        );
    }
}
