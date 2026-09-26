//! Hook tailer thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::collectors;
use crate::shared::config::SharedConfig;
use crate::shared::hooks::{self, ingest};

use super::{Sample, sleep_until};

/// Offsets are re-read from the DB each tick, so a batch the writer failed to commit is read again.
pub(super) fn hooks_loop(
    cfg: &SharedConfig,
    term: &AtomicBool,
    tx: &Sender<Sample>,
    db: Option<Connection>,
) {
    const TAIL_PERIOD: Duration = Duration::from_secs(2);
    let Some(db) = db else {
        tracing::error!("hook tailer has no database reader; job events will not be ingested");
        return;
    };
    let mut next = Instant::now();

    while !term.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            match reader::ingest_offsets(&db) {
                Ok(offsets) => {
                    let c = cfg.snapshot();
                    for r in collectors::runners::discover(&c.runner_roots) {
                        let stream = hooks::runner_event_log(&r.dir)
                            .to_string_lossy()
                            .into_owned();
                        let offset = offsets.get(&stream).copied().unwrap_or(0);
                        let (events, new_offset) = ingest::tail_events(&r.dir, &r.org, offset);
                        if new_offset == offset {
                            continue;
                        }
                        let batch = Sample::Hook {
                            stream,
                            runner: r.name,
                            events,
                            offset: new_offset,
                        };
                        if tx.send(batch).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "reading hook offsets"),
            }
            next = Instant::now() + TAIL_PERIOD;
        }
        sleep_until(next, term);
    }
}
