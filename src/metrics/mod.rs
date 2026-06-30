//! Prometheus exposition — `serve`'s reason to exist: sample → SQLite → expose.
//! Two independent, opt-in paths (config `[metrics]`): a pull `/metrics`
//! endpoint (loopback by default) and a JSON push to an ingestion sink. Both
//! read the DB on their own connections (WAL) — never the writer thread.

mod encode;
mod pull;
mod push;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use crate::config::Config;

/// Spawn the configured metrics threads; joined by `serve` on shutdown. No-op
/// if neither pull nor push is enabled.
pub fn spawn(cfg: &Config, term: Arc<AtomicBool>) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    if cfg.metrics.pull.enabled {
        handles.push(pull::spawn(cfg, Arc::clone(&term)));
    }
    if cfg.metrics.push.enabled {
        handles.push(push::spawn(cfg, Arc::clone(&term)));
    }
    handles
}
