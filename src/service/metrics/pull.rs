//! Prometheus pull endpoint (blocking `tiny_http`, loopback by default). Binds, rebinds or
//! closes as the live config changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;
use tiny_http::{Header, Response, Server};

use crate::service::metrics::encode::Snapshot;
use crate::service::store::open_reader;
use crate::shared::config::SharedConfig;
use crate::shared::util::now_epoch;

const TICK: Duration = Duration::from_millis(500);

pub fn spawn(shared: SharedConfig, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let version = env!("CARGO_PKG_VERSION");
    let db = shared.snapshot().db_path.clone(); // DB path is fixed for the run

    thread::Builder::new()
        .name("metrics-pull".into())
        .spawn(move || {
            let conn = open_reader(&db);
            let mut server: Option<Server> = None;
            let mut applied: Option<(bool, String)> = None;

            while !term.load(Ordering::SeqCst) {
                let cfg = shared.snapshot();
                let desired = (cfg.metrics.pull.enabled, cfg.metrics.pull.addr.clone());
                if applied.as_ref() != Some(&desired) {
                    server = None; // drop any existing listener first (closes the port)
                    if desired.0 {
                        match Server::http(&desired.1) {
                            Ok(s) => {
                                tracing::info!(addr = %desired.1, "metrics pull listening");
                                server = Some(s);
                            }
                            Err(e) => {
                                tracing::error!(error = %e, addr = %desired.1, "metrics pull: bind failed")
                            }
                        }
                    } else {
                        tracing::info!("metrics pull disabled");
                    }
                    applied = Some(desired);
                }

                match &server {
                    Some(s) => match s.recv_timeout(TICK) {
                        Ok(Some(req)) => {
                            let resp = if req.url().starts_with("/metrics") {
                                match body(conn.as_ref(), version, cfg.intervals.api_max_age())
                                {
                                    Ok(text) => {
                                        Response::from_string(text).with_header(text_header())
                                    }
                                    Err(e) => Response::from_string(format!("{e}\n"))
                                        .with_status_code(500),
                                }
                            } else {
                                Response::from_string("see /metrics\n")
                            };
                            let _ = req.respond(resp);
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "metrics pull: recv"),
                    },
                    None => thread::sleep(TICK),
                }
            }
            tracing::debug!("metrics pull stopped");
        })
        .expect("spawn metrics-pull")
}

/// The exposition, or why it could not be built (served as a 500 so the scrape fails).
fn body(conn: Option<&Connection>, version: &str, max_age: u64) -> Result<String, String> {
    let conn = conn.ok_or("database unavailable")?;
    Snapshot::gather(conn, now_epoch(), version, max_age)
        .map(|s| s.to_prometheus())
        .map_err(|e| format!("gather error: {e}"))
}

fn text_header() -> Header {
    Header::from_bytes(
        &b"Content-Type"[..],
        &b"text/plain; version=0.0.4; charset=utf-8"[..],
    )
    .expect("valid header")
}
