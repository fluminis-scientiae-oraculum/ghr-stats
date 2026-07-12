//! The collector — the systemd-managed `serve` service. It samples the fleet
//! into SQLite (Persistent mode's data source) and exposes it three ways: the
//! Prometheus `/metrics` endpoint, the JSON push, and the Unix-socket IPC the
//! TUI reads. It is NOT an interactive command — a TTY guard refuses a foreground
//! invocation and points at `ghr-stats systemd install`.
//!
//! Architecture: three producer threads feed a single DB-writer (the main
//! thread) over a bounded `crossbeam-channel`. No async — the work is blocking
//! I/O with no request/response concurrency to model.
//!
//! ```text
//!   local-sampler ─┐                             ┌─ metrics (pull/push)
//!   api-reconcile ─┼──(bounded)──►│ DB writer │──┤    own WAL reader conns
//!   hooks-tail   ──┘               owns Store     └─ ipc-server (TUI reads)
//!           ▲ all poll Arc<AtomicBool> (ctrlc-driven shutdown)
//! ```
//!
//! Why threads + a channel rather than one loop:
//! - The DB writer is the sole owner of the (non-`Sync`) SQLite `Connection`;
//!   samplers never touch it — they just send rows.
//! - The slow GitHub reconcile (network, seconds) runs independently of the
//!   fast local cadence, so it can never delay local sampling.
//! - The bounded channel gives natural backpressure if the writer falls behind.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::{Sender, bounded};
use nix::fcntl::{Flock, FlockArg};

use rusqlite::Connection;

use crate::service::store::{Store, open_reader, reader, writer};
use crate::shared::collectors::cpu::CpuRateTracker;
use crate::shared::collectors::{self};
use crate::shared::config::{Config, SharedConfig};
use crate::shared::hooks::ingest::HookEvent;
use crate::shared::models::{
    ApiRunnerRow, HostSample, JobConclusion, PendingConclusion, RunnerSample,
};
use crate::shared::util::now_epoch;

/// Walk the (expensive) `_work` trees once every N local ticks.
const WORK_WALK_EVERY: u64 = 12;
/// The daemon's lock file, beside the database.
fn lock_path(cfg: &Config) -> PathBuf {
    cfg.db_path.with_file_name("serve.lock")
}

/// Acquire the exclusive serve lock, held for the daemon's lifetime (dropped
/// when `run` returns, or when the process dies). Errors if another `serve`
/// already holds it — preventing a second DB writer.
fn acquire_lock(cfg: &Config) -> Result<Flock<std::fs::File>> {
    let path = lock_path(cfg);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening serve lock {}", path.display()))?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|(_, e)| anyhow!("another ghr-stats collector is already running ({e})"))
}

/// Granularity of the interruptible sleep between ticks.
const SLEEP_STEP: Duration = Duration::from_millis(200);
/// Channel depth — small; the writer keeps up, this just absorbs bursts.
const CHANNEL_BOUND: usize = 64;

/// One unit of work for the DB writer.
enum Sample {
    Local {
        runners: Vec<RunnerSample>,
        host: HostSample,
    },
    Api {
        ts: i64,
        rows: Vec<ApiRunnerRow>,
    },
    Hook {
        /// The tailed log's stream id (the per-runner event-log path).
        stream: String,
        events: Vec<HookEvent>,
        offset: u64,
    },
    JobConclusions {
        updates: Vec<JobConclusion>,
    },
}

pub fn run(cfg: &Config, config_override: Option<&Path>) -> Result<()> {
    // `serve` is the systemd-managed collector, not an interactive command:
    // refuse to run attached to a terminal (systemd gives the service no TTY) and
    // point at the installer. `GHR_STATS_ALLOW_TTY=1` is the dev/CI escape hatch.
    if std::io::stdin().is_terminal() && std::env::var_os("GHR_STATS_ALLOW_TTY").is_none() {
        bail!(
            "`serve` is the background collector, not an interactive command — \
             install it with `ghr-stats systemd install` \
             (set GHR_STATS_ALLOW_TTY=1 to run it in the foreground anyway)"
        );
    }

    // Single-writer guard: hold an exclusive advisory lock for the collector's
    // lifetime, so a second `serve` fails fast rather than double-writing the DB.
    // flock releases the instant the process dies — no stale lock.
    let _serve_lock = acquire_lock(cfg)?;
    let mut store = Store::open(&cfg.db_path)?;

    // SIGINT/SIGTERM/SIGHUP flip the flag; producers exit at the next check.
    let term = Arc::new(AtomicBool::new(false));
    {
        let term = Arc::clone(&term);
        ctrlc::set_handler(move || term.store(true, Ordering::SeqCst))
            .context("installing signal handler")?;
    }

    // With no configured roots, fall back to systemd-discovered ones (once) so
    // the collector finds the fleet even from a bare config.
    let mut initial = cfg.clone();
    initial.runner_roots = collectors::runners::effective_roots(&initial.runner_roots);
    // Live-reloadable config shared across the workers. An IPC mutation reloads it
    // in-process (see `ipc_server`), and each producer / metrics thread reads its
    // snapshot every cycle, so a change — a newly added PAT, a metrics toggle —
    // takes effect without a service restart.
    let shared = SharedConfig::new(initial);
    let (tx, rx) = bounded::<Sample>(CHANNEL_BOUND);

    let local = {
        let (cfg, term, tx) = (shared.clone(), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("local-sampler".into())
            .spawn(move || local_loop(&cfg, &term, &tx))
            .context("spawning local-sampler")?
    };
    let api = {
        // Its OWN WAL reader, used to find completed jobs still awaiting an API
        // conclusion (the writer owns the only writer connection).
        let reader = open_reader(&cfg.db_path);
        let (cfg, term, tx) = (shared.clone(), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("api-reconcile".into())
            .spawn(move || api_loop(&cfg, &term, &tx, reader))
            .context("spawning api-reconcile")?
    };
    let hooks = {
        // Resume tailing each runner's log from its last persisted offset. A
        // runner absent from this map (a new runner) is tailed from 0.
        let start_offsets = reader::ingest_offsets(store.conn()).unwrap_or_default();
        let (cfg, term, tx) = (shared.clone(), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("hooks-tail".into())
            .spawn(move || hooks_loop(&cfg, &term, &tx, start_offsets))
            .context("spawning hooks-tail")?
    };
    // Metrics exporter threads (pull/push): always spawned, each reconciles its
    // own resource to the live config (bind/drop the /metrics listener, post-or-
    // idle the push) — so `[metrics]` toggles take effect without a restart.
    let metrics = crate::service::metrics::spawn(&shared, Arc::clone(&term));
    // IPC server: serves the TUI's Persistent-mode history/jobs/GitHub over a
    // Unix socket (its own WAL reader connection), and reloads `shared` after an
    // authorized config mutation. This is what makes the collector reachable —
    // cross-scope included — without exposing the DB file.
    // The exact file config edits load from and write back to — an explicit
    // `--config`, else the canonical `/etc` path. Threaded into the IPC server so
    // an authorized mutation writes (and reloads) the SAME file `serve` loaded,
    // not a hardcoded `/etc` that a `--config` run never touched.
    let config_path = crate::shared::paths::config_write_target(config_override);
    let ipc = crate::service::ipc_server::spawn(&shared, Arc::clone(&term), config_path);

    // The writer holds only `rx`; once the producers exit and drop their
    // senders, `rx` disconnects and the loop below ends.
    drop(tx);

    {
        let cfg = shared.snapshot();
        tracing::info!(
            db = %cfg.db_path.display(),
            every_s = cfg.intervals.local_secs,
            api_every_s = cfg.intervals.api_secs,
            "serve started"
        );
    }

    for msg in rx.iter() {
        match msg {
            Sample::Local { runners, host } => {
                match writer::write_local(store.conn_mut(), &runners, &host) {
                    Ok(()) => tracing::debug!(runners = runners.len(), "local sample persisted"),
                    Err(e) => tracing::error!(error = %e, "local write failed"),
                }
            }
            Sample::Api { ts, rows } => {
                match writer::write_api_runners(store.conn_mut(), ts, &rows) {
                    Ok(()) => tracing::debug!(api_runners = rows.len(), "api reconcile persisted"),
                    Err(e) => tracing::error!(error = %e, "api write failed"),
                }
            }
            Sample::Hook {
                stream,
                events,
                offset,
            } => match writer::apply_hook_events(store.conn_mut(), &stream, &events, offset) {
                Ok(()) => {
                    tracing::debug!(stream = %stream, events = events.len(), offset, "hook events persisted")
                }
                Err(e) => tracing::error!(error = %e, stream = %stream, "hook write failed"),
            },
            Sample::JobConclusions { updates } => {
                match writer::apply_job_conclusions(store.conn_mut(), &updates) {
                    Ok(()) => tracing::debug!(n = updates.len(), "job conclusions reconciled"),
                    Err(e) => tracing::error!(error = %e, "job conclusion write failed"),
                }
            }
        }
    }

    let _ = local.join();
    let _ = api.join();
    let _ = hooks.join();
    for h in metrics {
        let _ = h.join();
    }
    let _ = ipc.join();
    tracing::info!("serve stopped");
    Ok(())
}

/// Producer: sample local sources on `local_secs`, deriving CPU% across ticks.
/// Reads the config snapshot each cycle, so a changed root/interval is picked up
/// live.
fn local_loop(cfg: &SharedConfig, term: &AtomicBool, tx: &Sender<Sample>) {
    let mut cpu = CpuRateTracker::new();
    let mut tick: u64 = 0;
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let c = cfg.snapshot();
            let now = now_epoch();
            let walk_work = tick.is_multiple_of(WORK_WALK_EVERY);
            let snap = collectors::collect_local(&c.runner_roots, now, walk_work);
            let runners = to_samples(snap.runners, now, &mut cpu);
            if tx
                .send(Sample::Local {
                    runners,
                    host: snap.host,
                })
                .is_err()
            {
                break; // writer gone
            }
            tick = tick.wrapping_add(1);
            next = Instant::now() + Duration::from_secs(c.intervals.local_secs.max(1));
        }
        sleep_until(next, term);
    }
}

/// Cap on how many pending job conclusions one reconcile cycle resolves — drains
/// a large backlog a batch at a time instead of a burst of API calls.
const JOB_RECONCILE_LIMIT: usize = 200;

/// Producer: reconcile GitHub's view on `api_secs`. Uses the explicit
/// `config.orgs` list when set, else discovers orgs from the runners' `.runner`
/// files each cycle — so it shares no mutable state with the local sampler. Each
/// cycle also resolves finished jobs' pass/fail `conclusion` from the Actions API
/// (opportunistic — see [`reconcile_job_conclusions`]), using its own reader.
fn api_loop(
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
            let rows = gather_api(&c, &orgs, term);
            if !rows.is_empty() && tx.send(Sample::Api { ts: now, rows }).is_err() {
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

/// Producer: tail every runner's OWN NDJSON job-event log and forward batches +
/// the advanced per-log offset. Each runner writes a log it owns (in its install
/// dir) and the collector reads it as root — so there is no shared-writable file
/// and no permission coordination. Runners are rediscovered each tick (the same
/// cheap `.runner` scan the local sampler does), so a runner added later is
/// picked up with no restart. Offsets are tracked in memory (seeded from the DB),
/// keyed by log path, so this thread shares no state with the writer.
fn hooks_loop(
    cfg: &SharedConfig,
    term: &AtomicBool,
    tx: &Sender<Sample>,
    mut offsets: HashMap<String, u64>,
) {
    const TAIL_PERIOD: Duration = Duration::from_secs(2);
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let c = cfg.snapshot();
            for r in collectors::runners::discover(&c.runner_roots) {
                let path = crate::shared::hooks::runner_event_log(&r.dir);
                let stream = path.to_string_lossy().into_owned();
                let offset = offsets.get(&stream).copied().unwrap_or(0);
                let (events, new_offset) = crate::shared::hooks::ingest::tail_events(&path, offset);
                if !events.is_empty() || new_offset != offset {
                    if tx
                        .send(Sample::Hook {
                            stream: stream.clone(),
                            events,
                            offset: new_offset,
                        })
                        .is_err()
                    {
                        return; // writer gone
                    }
                    offsets.insert(stream, new_offset);
                }
            }
            next = Instant::now() + TAIL_PERIOD;
        }
        sleep_until(next, term);
    }
}

/// Convert probes into storable samples, deriving CPU% from the usage delta.
fn to_samples(
    probes: Vec<collectors::RunnerProbe>,
    now: i64,
    cpu: &mut CpuRateTracker,
) -> Vec<RunnerSample> {
    let sampled_at = Instant::now();
    probes
        .into_iter()
        .map(|p| RunnerSample {
            ts: now,
            agent_id: p.info.agent_id,
            dir: p.info.dir.to_string_lossy().into_owned(),
            name: p.info.name,
            org: p.info.org,
            liveness: p.liveness,
            current_run_id: None,
            // Key CPU rate by the install dir (locally unique), NOT agent_id —
            // agentId is unique only within an org, so two runners in different
            // orgs sharing one would cross-contaminate their cgroup counters.
            cpu_pct: cpu.rate(p.info.dir.clone(), p.cpu_usage_usec, sampled_at),
            mem_bytes: p.mem_bytes,
            mem_current_bytes: p.mem_current_bytes,
            uptime_s: p.uptime_s,
        })
        .collect()
}

/// Query each org's runners (best-effort, per-org). A missing token, permission
/// error, or network failure degrades that org, never the cycle. Bails between
/// orgs if shutdown was signalled, so a SIGTERM mid-cycle exits promptly.
fn gather_api(cfg: &Config, orgs: &BTreeSet<String>, term: &AtomicBool) -> Vec<ApiRunnerRow> {
    let mut out = Vec::new();
    for org in orgs {
        if term.load(Ordering::SeqCst) {
            break;
        }
        let Some(token) = cfg.github_token_for(org) else {
            continue;
        };
        match crate::shared::github::list_org_runners(&token, org) {
            Ok(runners) => out.extend(runners.into_iter().map(|r| ApiRunnerRow {
                agent_id: r.id,
                org: org.clone(),
                name: r.name,
                online: r.status == "online",
                busy: r.busy,
            })),
            Err(e) => tracing::warn!(error = %e, org = %org, "api reconcile failed"),
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

/// Sleep until `deadline`, waking early (within `SLEEP_STEP`) when a signal
/// sets the terminate flag.
fn sleep_until(deadline: Instant, term: &AtomicBool) {
    while !term.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(SLEEP_STEP.min(deadline - now));
    }
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
