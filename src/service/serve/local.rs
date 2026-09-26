//! Local sampler thread. CPU% is a rate across ticks, so its [`CpuRateTracker`] lives on
//! this thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::shared::collectors::cpu::CpuRateTracker;
use crate::shared::collectors::{self};
use crate::shared::config::SharedConfig;
use crate::shared::models::RunnerSample;
use crate::shared::util::now_epoch;

use super::{Sample, WORK_WALK_EVERY, sleep_until};

pub(super) fn local_loop(cfg: &SharedConfig, term: &AtomicBool, tx: &Sender<Sample>) {
    let mut cpu = CpuRateTracker::new();
    // Starts at 1 so the first sample is not held up by a `_work` walk.
    let mut tick: u64 = 1;
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
                break;
            }
            tick = tick.wrapping_add(1);
            next = Instant::now() + Duration::from_secs(c.intervals.local_secs.max(1));
        }
        sleep_until(next, term);
    }
}

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
            // Keyed by install dir: GitHub's agentId is unique only within an org.
            cpu_pct: cpu.rate(&p.info.dir, p.cpu_usage_usec, sampled_at),
            mem_bytes: p.mem_bytes,
            mem_current_bytes: p.mem_current_bytes,
            uptime_s: p.uptime_s,
        })
        .collect()
}
