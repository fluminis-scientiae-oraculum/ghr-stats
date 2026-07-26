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
//!
//! That diagram is also the seam. One file per producer, and the WRITER stays
//! here — because [`run`] IS the writer: it owns the `Store`, and nothing else in
//! this module may touch it.
//!
//! - [`local`] — the local sampler thread, on `local_secs`.
//! - [`github`] — the reconcile thread, on `api_secs`, plus the job-conclusion
//!   backfill it opportunistically runs in the same cycle.
//! - [`jobs`] — the hook tailer, following each runner's own event log.
//!
//! It is the same cut already made in [`super::store::reader`],
//! [`super::store::writer`] and [`crate::shared::models`], which is what makes it
//! worth repeating: one producer's change lands in one file per layer, instead of
//! four files chosen on four different principles. [`Sample`] stays here because
//! it is the channel's vocabulary — the one thing all three producers and the
//! writer must agree on.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::bounded;
use nix::fcntl::{Flock, FlockArg};

use crate::service::store::{Store, open_reader, reader, writer};
use crate::shared::collectors::{self};
use crate::shared::config::{Config, SharedConfig};
use crate::shared::hooks::ingest::HookEvent;
use crate::shared::models::{ApiOrgOutcome, HostSample, JobConclusion, RunnerSample};

mod github;
mod jobs;
mod local;

use github::api_loop;
use jobs::hooks_loop;
use local::local_loop;

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
        outcomes: Vec<ApiOrgOutcome>,
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
            Sample::Api { ts, outcomes } => {
                match writer::write_api_runners(store.conn_mut(), ts, &outcomes) {
                    Ok(()) => {
                        tracing::debug!(orgs = outcomes.len(), "api reconcile persisted")
                    }
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
