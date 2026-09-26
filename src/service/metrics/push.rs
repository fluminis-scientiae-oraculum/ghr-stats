//! Metrics push: POSTs the snapshot as JSON to the configured ingestion endpoint on an
//! interval, following the live config.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::service::metrics::encode::Snapshot;
use crate::service::store::open_reader;
use crate::shared::config::SharedConfig;
use crate::shared::util::now_epoch;

const TICK: Duration = Duration::from_millis(200);

/// ureq's timeouts are infinite by default; a stalled endpoint would block SIGTERM shutdown.
const POST_TIMEOUT: Duration = Duration::from_secs(20);

pub fn spawn(shared: SharedConfig, term: Arc<AtomicBool>) -> JoinHandle<()> {
    let db = shared.snapshot().db_path.clone(); // DB path is fixed for the run
    let version = env!("CARGO_PKG_VERSION");

    thread::Builder::new()
        .name("metrics-push".into())
        .spawn(move || {
            let conn = open_reader(&db);
            let mut next = Instant::now();
            let mut active = false;

            while !term.load(Ordering::SeqCst) {
                let cfg = shared.snapshot();
                let push = &cfg.metrics.push;
                let on = push.enabled && !push.endpoint.is_empty();
                if on != active {
                    if on {
                        tracing::info!(endpoint = %redacted(&push.endpoint), every_s = push.interval_secs.max(5), "metrics push enabled");
                        if push.auth.is_some() && sends_auth_in_clear(&push.endpoint) {
                            tracing::warn!("metrics push sends its Authorization header over plain HTTP");
                        }
                        next = Instant::now();
                    } else {
                        tracing::info!("metrics push disabled");
                    }
                    active = on;
                }
                if on && Instant::now() >= next {
                    if let Some(conn) = conn.as_ref() {
                        match Snapshot::gather(
                            conn,
                            now_epoch(),
                            version,
                            cfg.intervals.api_max_age(),
                        ) {
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

/// `scheme://host/path` without userinfo or query, which may carry credentials.
fn redacted(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit('@').next().unwrap_or_default();
    format!("{scheme}://{host}/{path}")
}

/// Plain `http://` to anything but loopback.
fn sends_auth_in_clear(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    !matches!(host, "127.0.0.1" | "localhost" | "[::1]")
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
        Err(e) => tracing::warn!(
            error = %e.to_string().replace(endpoint, &redacted(endpoint)),
            "metrics push: POST failed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_endpoints_drop_credentials() {
        assert_eq!(
            redacted("https://user:pw@ingest.example.com/api/_json?token=abc"),
            "https://ingest.example.com/api/_json"
        );
        assert!(sends_auth_in_clear("http://ingest.example.com:5080/api"));
        assert!(!sends_auth_in_clear("http://127.0.0.1:5080/api"));
        assert!(!sends_auth_in_clear("https://ingest.example.com/api"));
    }
}
