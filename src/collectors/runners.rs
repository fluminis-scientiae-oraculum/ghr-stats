//! Runner discovery and live probing.
//!
//! Identity is read from each runner's own `.runner` file (authoritative);
//! liveness/resource use are derived from the runner's owning-uid processes and
//! its cgroup. Nothing here parses systemd unit names.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::cgroup;
use super::procscan::ProcInfo;
use crate::model::{Liveness, RunnerInfo};

/// Listener process kernel `comm` — present ⇒ runner online.
const LISTENER_COMM: &str = "Runner.Listener";
/// Worker process kernel `comm` — present ⇒ runner busy with a job.
const WORKER_COMM: &str = "Runner.Worker";

/// Raw shape of the `.runner` JSON we depend on.
#[derive(Debug, Deserialize)]
struct DotRunner {
    #[serde(rename = "agentId")]
    agent_id: i64,
    #[serde(rename = "agentName")]
    agent_name: String,
    #[serde(rename = "gitHubUrl")]
    github_url: String,
    #[serde(rename = "poolName")]
    pool_name: Option<String>,
    #[serde(rename = "workFolder")]
    work_folder: Option<String>,
}

/// A live probe of one runner, before CPU% (which needs two samples) is derived.
#[derive(Debug, Clone)]
pub struct RunnerProbe {
    pub info: RunnerInfo,
    pub liveness: Liveness,
    pub mem_bytes: Option<u64>,
    /// Cumulative cgroup CPU usage (µs); the daemon turns deltas into a percent.
    pub cpu_usage_usec: Option<u64>,
    pub uptime_s: Option<u64>,
}

/// Discover runners by scanning `roots` for subdirectories containing a
/// `.runner` file. Best-effort: a malformed or unreadable runner is logged and
/// skipped, never fatal.
pub fn discover(roots: &[PathBuf]) -> Vec<RunnerInfo> {
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            tracing::warn!(root = %root.display(), "runner root unreadable; skipping");
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let dot = dir.join(".runner");
            if !dot.is_file() {
                continue;
            }
            match read_runner(&dir, &dot) {
                Ok(info) => found.push(info),
                Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "skipping runner"),
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn read_runner(dir: &Path, dot: &Path) -> anyhow::Result<RunnerInfo> {
    let raw = std::fs::read_to_string(dot)?;
    let parsed: DotRunner = serde_json::from_str(strip_bom(&raw))?;
    let org = org_from_github_url(&parsed.github_url)
        .ok_or_else(|| anyhow::anyhow!("no org in gitHubUrl {:?}", parsed.github_url))?;
    let uid = std::fs::metadata(dir)?.uid();
    let user = uzers::get_user_by_uid(uid)
        .map(|u| u.name().to_string_lossy().into_owned())
        .unwrap_or_else(|| uid.to_string());
    Ok(RunnerInfo {
        agent_id: parsed.agent_id,
        name: parsed.agent_name,
        org,
        group: parsed.pool_name,
        dir: dir.to_path_buf(),
        work_folder: parsed.work_folder.unwrap_or_else(|| "_work".to_string()),
        uid,
        user,
    })
}

/// Probe every discovered runner against the current process snapshot.
pub fn probe_all(infos: Vec<RunnerInfo>, procs: &[ProcInfo], now_epoch: i64) -> Vec<RunnerProbe> {
    let boot = super::procscan::boot_time().unwrap_or(0);
    let clk_tck = clock_ticks();
    infos
        .into_iter()
        .map(|info| probe_one(info, procs, now_epoch, boot, clk_tck))
        .collect()
}

fn probe_one(
    info: RunnerInfo,
    procs: &[ProcInfo],
    now_epoch: i64,
    boot: i64,
    clk_tck: u64,
) -> RunnerProbe {
    let mine: Vec<&ProcInfo> = procs.iter().filter(|p| p.uid == info.uid).collect();
    let liveness = liveness_of(&mine);
    let listener = mine.iter().find(|p| p.comm == LISTENER_COMM);

    let (mem_bytes, cpu_usage_usec) = match listener.and_then(|p| cgroup::dir_for_pid(p.pid)) {
        Some(cg) => (cgroup::memory_current(&cg), cgroup::cpu_usage_usec(&cg)),
        None => (None, None),
    };
    let uptime_s = listener
        .and_then(|p| super::procscan::uptime_secs(now_epoch, boot, clk_tck, p.starttime_ticks));

    RunnerProbe {
        info,
        liveness,
        mem_bytes,
        cpu_usage_usec,
        uptime_s,
    }
}

/// Classify liveness from a runner's own processes.
fn liveness_of(mine: &[&ProcInfo]) -> Liveness {
    if mine.iter().any(|p| p.comm == WORKER_COMM) {
        Liveness::Busy
    } else if mine.iter().any(|p| p.comm == LISTENER_COMM) {
        Liveness::Idle
    } else {
        Liveness::Offline
    }
}

/// Strip a leading UTF-8 BOM — `.runner` files are written with one, which
/// `serde_json` would otherwise reject.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// Org (or owner) from a runner's `gitHubUrl`.
/// `https://github.com/pt-immer` → `pt-immer`;
/// `https://github.com/owner/repo` → `owner`.
fn org_from_github_url(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let mut segs = after_scheme.trim_end_matches('/').split('/');
    let _host = segs.next()?;
    match segs.next() {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    }
}

fn clock_ticks() -> u64 {
    // SAFETY: `sysconf` with a static, valid name has no preconditions; it
    // returns -1 (without setting errno for _SC_CLK_TCK) if unsupported.
    let v = unsafe { nix::libc::sysconf(nix::libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(uid: u32, comm: &str) -> ProcInfo {
        ProcInfo {
            pid: 1,
            uid,
            comm: comm.to_string(),
            starttime_ticks: 0,
        }
    }

    #[test]
    fn org_from_url_variants() {
        assert_eq!(
            org_from_github_url("https://github.com/pt-immer").as_deref(),
            Some("pt-immer")
        );
        assert_eq!(
            org_from_github_url("https://github.com/owner/repo").as_deref(),
            Some("owner")
        );
        assert_eq!(
            org_from_github_url("https://github.com/pt-immer/").as_deref(),
            Some("pt-immer")
        );
        assert_eq!(org_from_github_url("https://github.com/"), None);
        assert_eq!(org_from_github_url("https://github.com"), None);
    }

    #[test]
    fn bom_is_stripped() {
        let with_bom = "\u{feff}{\"x\":1}";
        assert_eq!(strip_bom(with_bom), "{\"x\":1}");
        assert_eq!(strip_bom("{\"x\":1}"), "{\"x\":1}");
    }

    #[test]
    fn dotrunner_parses_real_shape_with_bom() {
        let raw = "\u{feff}{\
            \"agentId\":83,\"agentName\":\"fso-epoch-immer-00\",\"poolId\":5,\
            \"poolName\":\"FSO Owned\",\"gitHubUrl\":\"https://github.com/pt-immer\",\
            \"workFolder\":\"_work\"}";
        let p: DotRunner = serde_json::from_str(strip_bom(raw)).unwrap();
        assert_eq!(p.agent_id, 83);
        assert_eq!(p.agent_name, "fso-epoch-immer-00");
        assert_eq!(
            org_from_github_url(&p.github_url).as_deref(),
            Some("pt-immer")
        );
        assert_eq!(p.pool_name.as_deref(), Some("FSO Owned"));
    }

    #[test]
    fn liveness_precedence() {
        assert_eq!(
            liveness_of(&[&proc(5, LISTENER_COMM), &proc(5, WORKER_COMM)]),
            Liveness::Busy
        );
        assert_eq!(liveness_of(&[&proc(5, LISTENER_COMM)]), Liveness::Idle);
        assert_eq!(liveness_of(&[&proc(5, "node")]), Liveness::Offline);
        assert_eq!(liveness_of(&[]), Liveness::Offline);
    }
}
