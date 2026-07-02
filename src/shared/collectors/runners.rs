//! Runner discovery and live probing.
//!
//! Identity is read from each runner's own `.runner` file (authoritative);
//! liveness/resource use are derived from the runner's owning-uid processes and
//! its cgroup. Nothing here derives identity from systemd unit names — root
//! auto-discovery only uses the `actions.runner.*` glob to LOCATE units, then
//! reads their `WorkingDirectory` property (still authoritative) for install
//! dirs.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::cgroup;
use super::procscan::ProcInfo;
use crate::shared::models::{Liveness, RunnerInfo};

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

/// Best-effort discovery of candidate runner-install ROOTS with no hint from the
/// user, by reading the `WorkingDirectory` of every `actions.runner.*` systemd
/// unit — the authoritative install-dir source — and returning the unique parent
/// dirs (a root is the dir that CONTAINS install dirs; see [`discover`]).
///
/// This locates units by a name glob but takes IDENTITY from nothing but the
/// unit property; the `.runner` file remains the identity source. Empty when
/// systemctl is unavailable or there are no runner units (the caller then falls
/// back to a manual prompt). System units only — the common `svc.sh install`
/// case; a user-scope-only setup still uses the manual path.
/// The runner roots to actually sample: the configured roots, or — when none are
/// configured — those auto-discovered from systemd's `actions.runner.*` units.
/// This is why a dashboard with no readable config still finds the fleet. Resolve
/// once at startup (it shells out to `systemctl`), never per sampling tick.
pub fn effective_roots(configured: &[PathBuf]) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    let discovered = discover_roots();
    if discovered.is_empty() {
        tracing::debug!("no runner_roots configured and no actions.runner.* units found");
    } else {
        tracing::info!(roots = ?discovered, "no runner_roots configured — discovered from systemd");
    }
    discovered
}

pub fn discover_roots() -> Vec<PathBuf> {
    let units = systemd_runner_units();
    if units.is_empty() {
        return Vec::new();
    }
    let show = Command::new("systemctl")
        .arg("show")
        .args(&units)
        .args(["--property", "WorkingDirectory"])
        .output();
    match show {
        Ok(o) => roots_from_workdirs(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

/// The `actions.runner.*.service` unit names known to systemd (loaded or not),
/// via systemd's structured JSON output (`--output=json`) — the stable machine
/// interface, so we parse a field, not a text column.
fn systemd_runner_units() -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Unit {
        unit: String,
    }
    let out = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--output=json",
            "actions.runner.*",
        ])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<Unit>>(&out.stdout)
        .unwrap_or_default()
        .into_iter()
        .map(|u| u.unit)
        .filter(|n| n.ends_with(".service"))
        .collect()
}

/// Parse `systemctl show --property WorkingDirectory` output into unique install
/// ROOTS (the parent of each `WorkingDirectory`). Pure, so it is unit-tested;
/// [`discover_roots`] only adds the shell-out.
fn roots_from_workdirs(show_output: &str) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for line in show_output.lines() {
        let Some(path) = line.trim().strip_prefix("WorkingDirectory=") else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() || path == "/" {
            continue;
        }
        if let Some(parent) = Path::new(path).parent() {
            let parent = parent.to_path_buf();
            if !roots.contains(&parent) {
                roots.push(parent);
            }
        }
    }
    roots.sort();
    roots
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

/// The runner's systemd unit name, read from its own `.service` file in the
/// install dir (authoritative — never parsed from a display string). `None` if
/// the file is absent or empty.
pub fn unit_name(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(".service"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

/// Liveness of the runner owning `uid`, from a process snapshot — the cheap
/// idle-gate for host-mutating ops (a one-shot `procscan::scan()` feeds every
/// runner). Same rule as [`probe_one`], so the gate can't disagree with the
/// dashboard's own liveness.
pub(crate) fn liveness_for(uid: u32, procs: &[ProcInfo]) -> Liveness {
    let mine: Vec<&ProcInfo> = procs.iter().filter(|p| p.uid == uid).collect();
    liveness_of(&mine)
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
/// `https://github.com/example-org` → `example-org`;
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
    // Safe wrapper over POSIX `sysconf` (nix) — no `unsafe`. The kernel's clock
    // tick rate, or the conventional 100 Hz fallback if it can't be read.
    match nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK) {
        Ok(Some(hz)) if hz > 0 => hz as u64,
        _ => 100,
    }
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
    fn roots_from_workdirs_dedups_parents_and_skips_junk() {
        // Two runners share a root; a third is elsewhere; blank / root / missing
        // WorkingDirectory lines are ignored. (Mirrors `systemctl show` output.)
        let out = "WorkingDirectory=/srv/runners/r0\n\
                   WorkingDirectory=/srv/runners/r1\n\
                   WorkingDirectory=\n\
                   WorkingDirectory=/\n\
                   WorkingDirectory=/opt/actions/solo\n";
        let roots = roots_from_workdirs(out);
        assert_eq!(
            roots,
            vec![PathBuf::from("/opt/actions"), PathBuf::from("/srv/runners"),]
        );
    }

    #[test]
    fn org_from_url_variants() {
        assert_eq!(
            org_from_github_url("https://github.com/example-org").as_deref(),
            Some("example-org")
        );
        assert_eq!(
            org_from_github_url("https://github.com/owner/repo").as_deref(),
            Some("owner")
        );
        assert_eq!(
            org_from_github_url("https://github.com/example-org/").as_deref(),
            Some("example-org")
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
            \"agentId\":42,\"agentName\":\"runner-01\",\"poolId\":5,\
            \"poolName\":\"Default Group\",\"gitHubUrl\":\"https://github.com/example-org\",\
            \"workFolder\":\"_work\"}";
        let p: DotRunner = serde_json::from_str(strip_bom(raw)).unwrap();
        assert_eq!(p.agent_id, 42);
        assert_eq!(p.agent_name, "runner-01");
        assert_eq!(
            org_from_github_url(&p.github_url).as_deref(),
            Some("example-org")
        );
        assert_eq!(p.pool_name.as_deref(), Some("Default Group"));
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
