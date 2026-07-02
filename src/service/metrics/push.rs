//! Metrics push: periodically POST the snapshot as JSON to a configured
//! ingestion endpoint (e.g. OpenObserve's `_json`). Blocking `ureq`; disabled
//! unless an endpoint is set. The interval loop polls the shutdown flag.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::service::metrics::encode::Snapshot;
use crate::service::store::open_reader;
use crate::shared::config::Config;
use crate::shared::util::now_epoch;

pub fn spawn(cfg: &Config, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let endpoint = cfg.metrics.push.endpoint.clone();
    let auth = cfg
        .metrics
        .push
        .auth
        .as_ref()
        .map(|s| s.expose().to_string());
    let interval = Duration::from_secs(cfg.metrics.push.interval_secs.max(5));
    let db = cfg.db_path.clone();
    let version = env!("CARGO_PKG_VERSION");

    thread::Builder::new()
        .name("metrics-push".into())
        .spawn(move || {
            if endpoint.is_empty() {
                tracing::warn!("metrics push enabled but endpoint is empty — disabled");
                return;
            }
            let Some(conn) = open_reader(&db) else {
                return;
            };
            tracing::info!(endpoint = %endpoint, every_s = interval.as_secs(), "metrics push enabled");

            let mut next = Instant::now();
            while !term.load(Ordering::SeqCst) {
                if Instant::now() >= next {
                    match Snapshot::gather(&conn, now_epoch(), version) {
                        Ok(s) => post(&endpoint, auth.as_deref(), &s.to_json()),
                        Err(e) => tracing::warn!(error = %e, "metrics push: gather"),
                    }
                    next = Instant::now() + interval;
                }
                thread::sleep(Duration::from_millis(200));
            }
            tracing::debug!("metrics push stopped");
        })
        .expect("spawn metrics-push")
}

fn post(endpoint: &str, auth: Option<&str>, body: &str) {
    let mut req = ureq::post(endpoint).set("Content-Type", "application/json");
    if let Some(a) = auth {
        req = req.set("Authorization", a);
    }
    match req.send_string(body) {
        Ok(_) => tracing::debug!("metrics pushed"),
        Err(e) => tracing::warn!(error = %e, "metrics push: POST failed"),
    }
}
