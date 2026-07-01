//! The `serve` daemon: sample the fleet into SQLite, and expose it as metrics.
//!
//! Architecture: three producer threads feed a single DB-writer (the main
//! thread) over a bounded `crossbeam-channel`. No async — the work is blocking
//! I/O with no request/response concurrency to model.
//!
//! ```text
//!   local-sampler ─┐
//!   api-reconcile ─┼──(bounded)──►│ DB writer │ owns Store ◄─reads─ metrics
//!   hooks-tail   ──┘                                                (pull/push)
//!           ▲ all poll Arc<AtomicBool> (ctrlc-driven shutdown)
//! ```
//!
//! Why threads + a channel rather than one loop:
//! - The DB writer is the sole owner of the (non-`Sync`) SQLite `Connection`;
//!   samplers never touch it — they just send rows.
//! - The slow GitHub reconcile (network, seconds) runs independently of the
//!   fast local cadence, so it can never delay local sampling.
//! - The bounded channel gives natural backpressure if the writer falls behind.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Sender, bounded};
use nix::fcntl::{Flock, FlockArg};

use crate::collectors::cpu::CpuRateTracker;
use crate::collectors::{self};
use crate::config::Config;
use crate::hooks::ingest::HookEvent;
use crate::model::{ApiRunnerRow, HostSample, RunnerSample};
use crate::store::{Store, reader, writer};
use crate::util::now_epoch;

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
        .map_err(|(_, e)| anyhow!("another `ghr-stats serve` is already running ({e})"))
}

/// Whether a `serve` daemon currently holds the lock — the TUI's honest
/// sampler-liveness check (no stale-pidfile / sample-age false positives).
pub(crate) fn is_running(cfg: &Config) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).open(lock_path(cfg)) else {
        return false; // no lock file ⇒ serve never started here
    };
    // A shared lock succeeds only if no one holds it exclusive; if `serve` holds
    // the exclusive lock, this fails (EWOULDBLOCK) ⇒ it's running.
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(_released_on_drop) => false,
        Err(_) => true,
    }
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
        events: Vec<HookEvent>,
        offset: u64,
    },
}

pub fn run(cfg: &Config) -> Result<()> {
    // Single-writer + a liveness signal in one: hold an exclusive advisory lock
    // for the daemon's lifetime. A second `serve` fails fast here; the TUI probes
    // this lock for an honest "sampler running?" (flock releases the instant the
    // process dies, so — unlike a pidfile or sample-age — there are no stale
    // false positives).
    let _serve_lock = acquire_lock(cfg)?;
    let mut store = Store::open(&cfg.db_path)?;

    // SIGINT/SIGTERM/SIGHUP flip the flag; producers exit at the next check.
    let term = Arc::new(AtomicBool::new(false));
    {
        let term = Arc::clone(&term);
        ctrlc::set_handler(move || term.store(true, Ordering::SeqCst))
            .context("installing signal handler")?;
    }

    let cfg = Arc::new(cfg.clone());
    let (tx, rx) = bounded::<Sample>(CHANNEL_BOUND);

    let local = {
        let (cfg, term, tx) = (Arc::clone(&cfg), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("local-sampler".into())
            .spawn(move || local_loop(&cfg, &term, &tx))
            .context("spawning local-sampler")?
    };
    let api = {
        let (cfg, term, tx) = (Arc::clone(&cfg), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("api-reconcile".into())
            .spawn(move || api_loop(&cfg, &term, &tx))
            .context("spawning api-reconcile")?
    };
    let hooks = {
        // Resume tailing from the last persisted offset.
        let start_offset = reader::ingest_offset(store.conn(), "hooks").unwrap_or(0);
        let (cfg, term, tx) = (Arc::clone(&cfg), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("hooks-tail".into())
            .spawn(move || hooks_loop(&cfg, &term, &tx, start_offset))
            .context("spawning hooks-tail")?
    };
    // Metrics exporter threads (pull/push) — opt-in; they read the DB on their
    // own WAL connections, never the writer. Empty if [metrics] is disabled.
    let metrics = crate::metrics::spawn(&cfg, Arc::clone(&term));

    // The writer holds only `rx`; once the producers exit and drop their
    // senders, `rx` disconnects and the loop below ends.
    drop(tx);

    tracing::info!(
        db = %cfg.db_path.display(),
        every_s = cfg.intervals.local_secs,
        api_every_s = cfg.intervals.api_secs,
        "serve started"
    );

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
            Sample::Hook { events, offset } => {
                match writer::apply_hook_events(store.conn_mut(), &events, offset) {
                    Ok(()) => {
                        tracing::debug!(events = events.len(), offset, "hook events persisted")
                    }
                    Err(e) => tracing::error!(error = %e, "hook write failed"),
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
    tracing::info!("serve stopped");
    Ok(())
}

/// Producer: sample local sources on `local_secs`, deriving CPU% across ticks.
fn local_loop(cfg: &Config, term: &AtomicBool, tx: &Sender<Sample>) {
    let mut cpu = CpuRateTracker::new();
    let period = Duration::from_secs(cfg.intervals.local_secs.max(1));
    let mut tick: u64 = 0;
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let now = now_epoch();
            let walk_work = tick.is_multiple_of(WORK_WALK_EVERY);
            let snap = collectors::collect_local(&cfg.runner_roots, now, walk_work);
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
            next = Instant::now() + period;
        }
        sleep_until(next, term);
    }
}

/// Producer: reconcile GitHub's view on `api_secs`. Uses the explicit
/// `config.orgs` list when set, else discovers orgs from the runners' `.runner`
/// files each cycle — so it shares no mutable state with the local sampler.
fn api_loop(cfg: &Config, term: &AtomicBool, tx: &Sender<Sample>) {
    let period = Duration::from_secs(cfg.intervals.api_secs.max(10));
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let orgs: BTreeSet<String> = if cfg.orgs.is_empty() {
                collectors::runners::discover(&cfg.runner_roots)
                    .into_iter()
                    .map(|r| r.org)
                    .collect()
            } else {
                cfg.orgs.iter().cloned().collect()
            };
            let now = now_epoch();
            let rows = gather_api(cfg, &orgs, term);
            if !rows.is_empty() && tx.send(Sample::Api { ts: now, rows }).is_err() {
                break; // writer gone
            }
            next = Instant::now() + period;
        }
        sleep_until(next, term);
    }
}

/// Producer: tail the NDJSON job-event log and forward batches + the advanced
/// offset. Tracks the offset in memory (seeded from the DB) so it shares no
/// state with the writer.
fn hooks_loop(cfg: &Config, term: &AtomicBool, tx: &Sender<Sample>, mut offset: u64) {
    const TAIL_PERIOD: Duration = Duration::from_secs(2);
    let path = cfg.event_log.clone();
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let (events, new_offset) = crate::hooks::ingest::tail_events(&path, offset);
            if (!events.is_empty() || new_offset != offset)
                && tx
                    .send(Sample::Hook {
                        events,
                        offset: new_offset,
                    })
                    .is_err()
            {
                break; // writer gone
            }
            offset = new_offset;
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
            name: p.info.name,
            org: p.info.org,
            liveness: p.liveness,
            current_run_id: None,
            cpu_pct: cpu.rate(p.info.agent_id, p.cpu_usage_usec, sampled_at),
            mem_bytes: p.mem_bytes,
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
        match crate::github::list_org_runners(&token, org) {
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
