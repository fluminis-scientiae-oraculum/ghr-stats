//! The runner job-event hook boundary.
//!
//! `ingest` tails the append-only NDJSON log the hooks write; `install`
//! (detect / chain / instruct, P6) manages the hook scripts. Kept separate
//! from `collectors` on purpose: these read and manage the runner-hook
//! contract, they do not sample host resources.

pub mod env;
pub mod ingest;
pub mod install;
pub mod uninstall;
