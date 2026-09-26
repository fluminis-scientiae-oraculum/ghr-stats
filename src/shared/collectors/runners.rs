//! Runner discovery and live probing. Identity comes from each runner's own
//! `.runner` file; liveness and resource use from the processes whose `argv[0]`
//! lies under its install dir, and their cgroup.

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::cgroup;
use super::procscan::{self, ProcInfo};
use crate::shared::models::{Liveness, RunnerInfo};
use crate::shared::runner_files;

/// Listener process kernel `comm` — present ⇒ runner online.
const LISTENER_COMM: &str = "Runner.Listener";
/// Worker process kernel `comm` — present ⇒ runner busy with a job.
const WORKER_COMM: &str = "Runner.Worker";

const DOT_RUNNER_CAP: u64 = 64 * 1024;
const DOT_SERVICE_CAP: u64 = 1024;

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
    /// Working-set memory (anon+shmem) — the runner's true resident footprint.
    pub mem_bytes: Option<u64>,
    /// Raw cgroup `memory.current` (working set + reclaimable page cache), kept
    /// for the `ghr_runner_mem_current_bytes` gauge / cache-vs-working-set view.
    pub mem_current_bytes: Option<u64>,
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
            if !dir.join(".runner").exists() {
                continue;
            }
            match read_runner(&dir) {
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

fn read_runner(dir: &Path) -> anyhow::Result<RunnerInfo> {
    let (raw, _) = runner_files::read_text(dir, ".runner", DOT_RUNNER_CAP)?;
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
        user,
    })
}

/// A runner's systemd unit: named like a runner unit and serving that runner's
/// install dir. Built only by [`unit_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunnerUnit(String);

impl RunnerUnit {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl fmt::Display for RunnerUnit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The unit named by `dir/.service`, if systemd confirms it runs from `dir`.
pub(crate) fn unit_for(dir: &Path) -> Result<RunnerUnit, String> {
    let (text, _) = runner_files::read_text(dir, ".service", DOT_SERVICE_CAP)
        .map_err(|e| format!("no usable .service file ({e})"))?;
    let name = text.trim();
    if !is_runner_unit_name(name) {
        return Err(format!("{name:?} is not an actions.runner.*.service unit"));
    }
    let out = Command::new("systemctl")
        .args(["show", "-P", "WorkingDirectory", "--", name])
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    let workdir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !same_dir(&workdir, dir) {
        return Err(format!(
            "{name} runs from {}, not {}",
            workdir.display(),
            dir.display()
        ));
    }
    Ok(RunnerUnit(name.to_string()))
}

fn is_runner_unit_name(name: &str) -> bool {
    name.strip_prefix("actions.runner.")
        .and_then(|rest| rest.strip_suffix(".service"))
        .is_some_and(|mid| {
            !mid.is_empty()
                && mid
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-@:\\".contains(&b))
        })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
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
    let mine = processes_of(&info.dir, procs);
    let liveness = liveness_of(&mine);
    let listener = mine.iter().find(|p| p.comm == LISTENER_COMM);

    let (mem_bytes, mem_current_bytes, cpu_usage_usec) =
        match listener.and_then(|p| cgroup::dir_for_pid(p.pid)) {
            Some(cg) => (
                cgroup::memory_working_set(&cg),
                cgroup::memory_current(&cg),
                cgroup::cpu_usage_usec(&cg),
            ),
            None => (None, None, None),
        };
    let uptime_s = listener
        .and_then(|p| super::procscan::uptime_secs(now_epoch, boot, clk_tck, p.starttime_ticks));

    RunnerProbe {
        info,
        liveness,
        mem_bytes,
        mem_current_bytes,
        cpu_usage_usec,
        uptime_s,
    }
}

/// Liveness of the runner installed at `dir`, from a process snapshot. Same rule
/// as [`probe_all`], so an idle gate cannot disagree with the dashboard.
pub(crate) fn liveness_in(dir: &Path, procs: &[ProcInfo]) -> Liveness {
    liveness_of(&processes_of(dir, procs))
}

/// Whether the runner at `dir` is idle right now, from a fresh process scan.
pub(crate) fn is_idle_now(dir: &Path) -> bool {
    liveness_in(dir, &procscan::scan()) == Liveness::Idle
}

/// Runner processes whose `argv[0]` lies under `dir`. The runner launches its
/// binaries by path, sometimes through `dir/bin` and sometimes through the
/// versioned dir that symlink resolves to, so both spellings of `dir` count.
fn processes_of<'a>(dir: &Path, procs: &'a [ProcInfo]) -> Vec<&'a ProcInfo> {
    let real = std::fs::canonicalize(dir).ok();
    procs
        .iter()
        .filter(|p| {
            p.argv0.as_deref().is_some_and(|a| {
                a.starts_with(dir) || real.as_deref().is_some_and(|r| a.starts_with(r))
            })
        })
        .collect()
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

    fn proc(comm: &str, argv0: &str) -> ProcInfo {
        ProcInfo {
            pid: 1,
            comm: comm.to_string(),
            argv0: Some(PathBuf::from(argv0)),
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
    fn processes_are_attributed_by_install_dir_through_the_bin_symlink() {
        let root = tempfile::tempdir().unwrap();
        let r0 = root.path().join("r0");
        let r00 = root.path().join("r00");
        std::fs::create_dir_all(r0.join("bin.2.0.0")).unwrap();
        std::fs::create_dir_all(&r00).unwrap();
        std::os::unix::fs::symlink(r0.join("bin.2.0.0"), r0.join("bin")).unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();

        let listener = proc(LISTENER_COMM, &s(&r0.join("bin/Runner.Listener")));
        let worker = proc(WORKER_COMM, &s(&r0.join("bin.2.0.0/Runner.Worker")));
        let sibling = proc(WORKER_COMM, &s(&r00.join("bin/Runner.Worker")));
        let procs = [listener, worker, sibling];

        assert_eq!(liveness_in(&r0, &procs), Liveness::Busy);
        assert_eq!(liveness_in(&r00, &procs[..2]), Liveness::Offline);
        assert_eq!(liveness_in(&r0, &procs[..1]), Liveness::Idle);
    }

    #[test]
    fn only_runner_unit_names_pass() {
        assert!(is_runner_unit_name(
            "actions.runner.example-org.runner-01.service"
        ));
        assert!(is_runner_unit_name(
            "actions.runner.example-org.my\\x2drunner.service"
        ));
        for bad in [
            "poweroff.target",
            "actions.runner..service",
            "actions.runner.x.service extra",
            "actions.runner.a/b.service",
            "ssh.service",
        ] {
            assert!(!is_runner_unit_name(bad), "{bad}");
        }
    }
}
