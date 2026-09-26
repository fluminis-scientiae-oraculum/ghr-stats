//! Prometheus exposition: a pull `/metrics` endpoint and a JSON push, both opt-in via `[metrics]`.
//! Both read the DB on their own WAL connections, never the writer thread.

pub mod encode;

pub use encode::Snapshot;
mod pull;
mod push;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::JoinHandle;

use crate::shared::config::SharedConfig;

/// Both threads always spawn and reconcile to the live config each cycle, so a `[metrics]`
/// toggle needs no restart.
pub fn spawn(shared: &SharedConfig, term: Arc<AtomicBool>) -> Vec<JoinHandle<()>> {
    vec![
        pull::spawn(shared.clone(), Arc::clone(&term)),
        push::spawn(shared.clone(), Arc::clone(&term)),
    ]
}
