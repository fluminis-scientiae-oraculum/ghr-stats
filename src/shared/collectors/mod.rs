//! Data collectors. Local sources (runners, host) are synchronous blocking
//! `/proc`, `/sys`, and `statvfs` reads; the GitHub collector uses the blocking
//! `ureq` client. The daemon calls them directly — the whole tool is sync.

mod cgroup;
pub mod cpu;
pub mod host;
pub(crate) mod procscan;
pub mod runners;

use std::path::PathBuf;

use crate::shared::models::HostSample;
pub use runners::RunnerProbe;

/// One round of local sampling.
pub struct LocalSnapshot {
    pub runners: Vec<RunnerProbe>,
    pub host: HostSample,
}

/// Discover + probe runners and sample the host in one blocking pass.
/// Best-effort throughout: a failing source degrades that field, never the run.
pub fn collect_local(roots: &[PathBuf], now_epoch: i64, walk_work: bool) -> LocalSnapshot {
    let infos = runners::discover(roots);
    let work_dirs: Vec<PathBuf> = infos.iter().map(|i| i.dir.join(&i.work_folder)).collect();
    let procs = procscan::scan();
    let runners = runners::probe_all(infos, &procs, now_epoch);
    let host = host::sample(now_epoch, roots, walk_work.then_some(work_dirs.as_slice()));
    LocalSnapshot { runners, host }
}
