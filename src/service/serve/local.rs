//! The local sampler thread: what this host can see about itself.
//!
//! Runs on `local_secs`, the fast cadence, and is deliberately independent of the
//! GitHub reconcile — a network call that takes seconds must never delay a tick
//! that takes milliseconds. It re-reads the config snapshot each cycle, so a
//! changed root or interval is picked up live without a restart.
//!
//! CPU% is a RATE, so it only exists across ticks: [`super::run`] cannot derive it
//! and neither can a single probe. The [`CpuRateTracker`] therefore lives in this
//! thread's own stack, which is also why nothing else needs to see it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::shared::collectors::cpu::CpuRateTracker;
use crate::shared::collectors::{self};
use crate::shared::config::SharedConfig;
use crate::shared::models::RunnerSample;
use crate::shared::util::now_epoch;

use super::{Sample, WORK_WALK_EVERY, sleep_until};

/// Producer: sample local sources on `local_secs`, deriving CPU% across ticks.
/// Reads the config snapshot each cycle, so a changed root/interval is picked up
/// live.
pub(super) fn local_loop(cfg: &SharedConfig, term: &AtomicBool, tx: &Sender<Sample>) {
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
