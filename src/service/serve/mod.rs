//! The systemd-managed collector. Producer threads ([`local`], [`github`], [`jobs`])
//! send [`Sample`]s over a bounded channel to [`run`], the sole owner of the SQLite
//! writer; metrics and IPC threads read on their own WAL connections.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::bounded;
use nix::fcntl::{Flock, FlockArg};

use crate::service::store::{open_reader, open_writer, writer};
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

/// In local ticks; the `_work` walk is expensive.
const WORK_WALK_EVERY: u64 = 12;

fn lock_path(cfg: &Config) -> PathBuf {
    cfg.db_path.with_file_name("serve.lock")
}

/// Prevents a second DB writer. The kernel drops a `flock` when its holder dies, so no stale lock.
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

const SLEEP_STEP: Duration = Duration::from_millis(200);
const CHANNEL_BOUND: usize = 64;

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
        /// The tailed log's path; keys its offset.
        stream: String,
        runner: String,
        events: Vec<HookEvent>,
        offset: u64,
    },
    JobConclusions {
        updates: Vec<JobConclusion>,
    },
}

pub fn run(cfg: &Config, config_override: Option<&Path>) -> Result<()> {
    if std::io::stdin().is_terminal() && std::env::var_os("GHR_STATS_ALLOW_TTY").is_none() {
        bail!(
            "`serve` is the background collector, not an interactive command — \
             install it with `ghr-stats systemd install` \
             (set GHR_STATS_ALLOW_TTY=1 to run it in the foreground anyway)"
        );
    }

    let _serve_lock = acquire_lock(cfg)?;
    let mut db = open_writer(&cfg.db_path)?;
    let sock = crate::service::ipc_server::socket_path();
    let listener = crate::service::ipc_server::bind(&sock)
        .with_context(|| format!("binding the IPC socket {}", sock.display()))?;

    // ctrlc's `termination` feature covers SIGINT, SIGTERM and SIGHUP.
    let term = Arc::new(AtomicBool::new(false));
    {
        let term = Arc::clone(&term);
        ctrlc::set_handler(move || term.store(true, Ordering::SeqCst))
            .context("installing signal handler")?;
    }

    let mut initial = cfg.clone();
    initial.runner_roots = collectors::runners::effective_roots(&initial.runner_roots);
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
        let reader = open_reader(&cfg.db_path);
        let (cfg, term, tx) = (shared.clone(), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("api-reconcile".into())
            .spawn(move || api_loop(&cfg, &term, &tx, reader))
            .context("spawning api-reconcile")?
    };
    let hooks = {
        let reader = open_reader(&cfg.db_path);
        let (cfg, term, tx) = (shared.clone(), Arc::clone(&term), tx.clone());
        thread::Builder::new()
            .name("hooks-tail".into())
            .spawn(move || hooks_loop(&cfg, &term, &tx, reader))
            .context("spawning hooks-tail")?
    };
    let metrics = crate::service::metrics::spawn(&shared, Arc::clone(&term));
    // The file `serve` loaded, so a mutation under `--config` never writes `/etc`.
    let config_path = crate::shared::paths::config_write_target(config_override);
    let ipc = crate::service::ipc_server::spawn(listener, &shared, Arc::clone(&term), config_path);

    // Producers hold the remaining senders; `rx` ends once they exit.
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
                match writer::write_local(&mut db, &runners, &host) {
                    Ok(()) => tracing::debug!(runners = runners.len(), "local sample persisted"),
                    Err(e) => tracing::error!(error = %e, "local write failed"),
                }
            }
            Sample::Api { ts, outcomes } => {
                match writer::write_api_runners(&mut db, ts, &outcomes) {
                    Ok(()) => {
                        tracing::debug!(orgs = outcomes.len(), "api reconcile persisted")
                    }
                    Err(e) => tracing::error!(error = %e, "api write failed"),
                }
            }
            Sample::Hook {
                stream,
                runner,
                events,
                offset,
            } => match writer::apply_hook_events(&mut db, &stream, &runner, &events, offset) {
                Ok(()) => {
                    tracing::debug!(stream = %stream, events = events.len(), offset, "hook events persisted")
                }
                Err(e) => tracing::error!(error = %e, stream = %stream, "hook write failed"),
            },
            Sample::JobConclusions { updates } => {
                match writer::apply_job_conclusions(&mut db, &updates) {
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

fn sleep_until(deadline: Instant, term: &AtomicBool) {
    while !term.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        thread::sleep(SLEEP_STEP.min(deadline - now));
    }
}
