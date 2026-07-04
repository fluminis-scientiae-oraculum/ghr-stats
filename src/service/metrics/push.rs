//! Metrics push: periodically POST the snapshot as JSON to a configured
//! ingestion endpoint (e.g. OpenObserve's `_json`). Blocking `ureq`. The thread
//! reads the live config each cycle: it posts on the interval when enabled with
//! an endpoint, and idles otherwise — so enabling/disabling push (or changing
//! the endpoint) takes effect without a restart. Enable/disable transitions are
//! logged once, not per cycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::service::metrics::encode::Snapshot;
use crate::service::store::open_reader;
use crate::shared::config::SharedConfig;
use crate::shared::util::now_epoch;

/// Poll granularity for shutdown + config changes while idle/between posts.
const TICK: Duration = Duration::from_millis(200);

/// Read/write timeout for the push POST. ureq leaves these infinite by default,
/// so a stalled ingestion endpoint would hang this thread and block the
/// collector's SIGTERM shutdown. Bound it.
const POST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn spawn(shared: SharedConfig, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let db = shared.snapshot().db_path.clone(); // DB path is fixed for the run
    let version = env!("CARGO_PKG_VERSION");

    thread::Builder::new()
        .name("metrics-push".into())
        .spawn(move || {
            let conn = open_reader(&db);
            let mut next = Instant::now();
            let mut active = false; // whether push is currently on — for one-shot transition logs

            while !term.load(Ordering::SeqCst) {
                let cfg = shared.snapshot();
                let push = &cfg.metrics.push;
                let on = push.enabled && !push.endpoint.is_empty();
                if on != active {
                    if on {
                        tracing::info!(endpoint = %push.endpoint, every_s = push.interval_secs.max(5), "metrics push enabled");
                        next = Instant::now(); // post promptly on enable
                    } else {
                        tracing::info!("metrics push disabled");
                    }
                    active = on;
                }
                if on && Instant::now() >= next {
                    if let Some(conn) = conn.as_ref() {
                        match Snapshot::gather(conn, now_epoch(), version) {
                            Ok(s) => post(&push.endpoint, push.auth.as_ref().map(|a| a.expose()), &s.to_json()),
                            Err(e) => tracing::warn!(error = %e, "metrics push: gather"),
                        }
                    }
                    next = Instant::now() + Duration::from_secs(push.interval_secs.max(5));
                }
                thread::sleep(TICK);
            }
            tracing::debug!("metrics push stopped");
        })
        .expect("spawn metrics-push")
}

fn post(endpoint: &str, auth: Option<&str>, body: &str) {
    let mut req = ureq::post(endpoint)
        .config()
        .timeout_global(Some(POST_TIMEOUT))
        .build()
        .header("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }
    match req.send(body) {
        Ok(_) => tracing::debug!("metrics pushed"),
        Err(e) => tracing::warn!(error = %e, "metrics push: POST failed"),
    }
}
