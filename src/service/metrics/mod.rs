//! Prometheus exposition — one of the collector's outputs: sample → SQLite → expose.
//! Two independent, opt-in paths (config `[metrics]`): a pull `/metrics`
//! endpoint (loopback by default) and a JSON push to an ingestion sink. Both
//! read the DB on their own connections (WAL) — never the writer thread.

mod encode;
mod pull;
mod push;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use crate::shared::config::SharedConfig;

/// Spawn the metrics threads, joined by `serve` on shutdown. Both are ALWAYS
/// spawned (not gated on the startup config): each reconciles its own resource
/// to the live config snapshot every cycle — the pull thread binds/drops its
/// `/metrics` listener, the push thread posts-or-idles — so a `[metrics]` toggle
/// via the TUI takes effect without a restart. A disabled thread just idles.
pub fn spawn(shared: &SharedConfig, term: Arc<AtomicBool>) -> Vec<JoinHandle<()>> {
    vec![
        pull::spawn(shared.clone(), Arc::clone(&term)),
        push::spawn(shared.clone(), Arc::clone(&term)),
    ]
}
