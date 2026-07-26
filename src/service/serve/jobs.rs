//! The hook tailer thread: what the runners' own hooks reported.
//!
//! Each runner writes an NDJSON event log in its OWN install dir and the collector
//! reads it as root, so there is no shared-writable file and no permission
//! coordination — the design that replaced a single shared log which silently
//! dropped events whenever its path was not writable.
//!
//! Runners are rediscovered every tick by the same cheap `.runner` scan the local
//! sampler does, so a runner added later is picked up with no restart. Offsets are
//! held in memory, seeded once from the DB and keyed by log path, which is why
//! this thread shares no state with the writer: it sends the advanced offset along
//! with the batch and lets the writer persist both in one transaction.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::shared::collectors::{self};
use crate::shared::config::SharedConfig;

use super::{Sample, sleep_until};

/// Producer: tail every runner's OWN NDJSON job-event log and forward batches +
/// the advanced per-log offset. Each runner writes a log it owns (in its install
/// dir) and the collector reads it as root — so there is no shared-writable file
/// and no permission coordination. Runners are rediscovered each tick (the same
/// cheap `.runner` scan the local sampler does), so a runner added later is
/// picked up with no restart. Offsets are tracked in memory (seeded from the DB),
/// keyed by log path, so this thread shares no state with the writer.
pub(super) fn hooks_loop(
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
