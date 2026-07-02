//! Prometheus pull endpoint: a tiny blocking HTTP server. Bound to loopback by
//! default (see `PullConfig::addr`). The thread reconciles its listener to the
//! live config each cycle — binding when enabled, dropping it (closing the port)
//! when disabled, rebinding on an address change — so a `[m]` toggle in the TUI
//! takes effect without a restart. The recv loop uses a short timeout so it
//! observes both the shutdown flag and config changes promptly.

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

/// How long a bound listener blocks on `recv` (also the disabled-state poll) —
/// bounds how quickly the thread reacts to shutdown or a config change.
const TICK: Duration = Duration::from_millis(500);

pub fn spawn(shared: SharedConfig, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let version = env!("CARGO_PKG_VERSION");
    let db = shared.snapshot().db_path.clone(); // DB path is fixed for the run

    thread::Builder::new()
        .name("metrics-pull".into())
        .spawn(move || {
            let conn = open_reader(&db);
            let mut server: Option<Server> = None;
            // The last-reconciled desired state; act only on a transition, so a
            // steady state neither rebinds nor spams the log.
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
                            // Leave unbound; a later addr change retries. Logged once
                            // (this is a transition), so no per-cycle spam.
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
                                Response::from_string(body(conn.as_ref(), version))
                                    .with_header(text_header())
                            } else {
                                Response::from_string("see /metrics\n")
                            };
                            let _ = req.respond(resp);
                        }
                        Ok(None) => {} // timeout — re-check shutdown + config
                        Err(e) => tracing::warn!(error = %e, "metrics pull: recv"),
                    },
                    None => thread::sleep(TICK), // disabled/unbound — poll config + term
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
