//! Prometheus pull endpoint: a tiny blocking HTTP server. Bound to loopback by
//! default (see `PullConfig::addr`). The single-threaded recv loop uses a short
//! timeout so it observes the shutdown flag promptly.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;
use tiny_http::{Header, Response, Server};

use crate::service::metrics::encode::Snapshot;
use crate::service::store::open_reader;
use crate::shared::config::Config;
use crate::shared::util::now_epoch;

pub fn spawn(cfg: &Config, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let addr = cfg.metrics.pull.addr.clone();
    let db = cfg.db_path.clone();
    let version = env!("CARGO_PKG_VERSION");

    thread::Builder::new()
        .name("metrics-pull".into())
        .spawn(move || {
            let server = match Server::http(&addr) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, addr = %addr, "metrics pull: bind failed");
                    return;
                }
            };
            tracing::info!(addr = %addr, "metrics pull listening");

            let conn = open_reader(&db);
            while !term.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(500)) {
                    Ok(Some(req)) => {
                        let resp = if req.url().starts_with("/metrics") {
                            Response::from_string(body(conn.as_ref(), version))
                                .with_header(text_header())
                        } else {
                            Response::from_string("see /metrics\n")
                        };
                        let _ = req.respond(resp);
                    }
                    Ok(None) => {} // timeout — re-check shutdown
                    Err(e) => tracing::warn!(error = %e, "metrics pull: recv"),
                }
            }
            tracing::debug!("metrics pull stopped");
        })
        .expect("spawn metrics-pull")
}

fn body(conn: Option<&Connection>, version: &str) -> String {
    let Some(conn) = conn else {
        return "# db unavailable\n".to_string();
    };
    match Snapshot::gather(conn, now_epoch(), version) {
        Ok(s) => s.to_prometheus(),
        Err(e) => format!("# gather error: {e}\n"),
    }
}

fn text_header() -> Header {
    Header::from_bytes(
        &b"Content-Type"[..],
        &b"text/plain; version=0.0.4; charset=utf-8"[..],
    )
    .expect("valid header")
}
