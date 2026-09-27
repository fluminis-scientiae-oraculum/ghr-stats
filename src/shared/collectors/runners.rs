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
use crate::shared::github::RunnerScope;
use crate::shared::models::{Liveness, RunnerInfo};
use crate::shared::runner_files;

const LISTENER_COMM: &str = "Runner.Listener";
const WORKER_COMM: &str = "Runner.Worker";

const DOT_RUNNER_CAP: u64 = 64 * 1024;
const DOT_SERVICE_CAP: u64 = 1024;

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
    /// Working-set memory (anon + shmem).
    pub mem_bytes: Option<u64>,
    /// Raw cgroup `memory.current` (working set + reclaimable page cache).
    pub mem_current_bytes: Option<u64>,
    /// Cumulative cgroup CPU usage (µs); the daemon turns deltas into a percent.
    pub cpu_usage_usec: Option<u64>,
    pub uptime_s: Option<u64>,
}

/// Install dirs are the subdirectories of `roots` holding a `.runner` file; bad ones are
/// logged and skipped.
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

/// Configured roots, else those discovered from systemd. Shells out, so resolve once at startup.
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

/// Parents of the `WorkingDirectory` of every system `actions.runner.*` unit; empty without
/// systemctl.
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
    let scope = RunnerScope::parse(&parsed.github_url).map_err(anyhow::Error::msg)?;
    let uid = std::fs::metadata(dir)?.uid();
    let user = uzers::get_user_by_uid(uid)
        .map(|u| u.name().to_string_lossy().into_owned())
        .unwrap_or_else(|| uid.to_string());
    Ok(RunnerInfo {
        agent_id: parsed.agent_id,
        name: parsed.agent_name,
        org: scope.login().to_string(),
        scope,
        group: parsed.pool_name,
        dir: dir.to_path_buf(),
        work_folder: parsed.work_folder.unwrap_or_else(|| "_work".to_string()),
        user,
    })
}

/// A runner-named unit confirmed to run from the runner's install dir; built only by [`unit_for`].
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

/// A runner's `_temp` and `_diag`, and the user who owns its install dir; built only by
/// [`scratch_for`], so a purge can name no other path or user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunnerScratch {
    temp: PathBuf,
    diag: PathBuf,
    uid: u32,
    gid: u32,
}

impl RunnerScratch {
    pub(crate) fn temp(&self) -> &Path {
        &self.temp
    }

    pub(crate) fn diag(&self) -> &Path {
        &self.diag
    }

    pub(crate) fn uid(&self) -> u32 {
        self.uid
    }

    pub(crate) fn gid(&self) -> u32 {
        self.gid
    }

    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path, uid: u32) -> Self {
        Self {
            temp: dir.join("_work/_temp"),
            diag: dir.join("_diag"),
            uid,
            gid: uid,
        }
    }
}

/// The scratch of the runner installed at `dir`, from its own `.runner`. The runner
/// writes `_diag` at the install root, beside the work folder, not inside it.
pub(crate) fn scratch_for(dir: &Path) -> Result<RunnerScratch, String> {
    let info = read_runner(dir).map_err(|e| format!("no usable .runner ({e})"))?;
    let owner = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(RunnerScratch {
        temp: dir.join(&info.work_folder).join("_temp"),
        diag: dir.join("_diag"),
        uid: owner.uid(),
        gid: owner.gid(),
    })
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

/// Same rule as [`probe_all`], so an idle gate can't disagree with the dashboard.
pub(crate) fn liveness_in(dir: &Path, procs: &[ProcInfo]) -> Liveness {
    liveness_of(&processes_of(dir, procs))
}

pub(crate) fn is_idle_now(dir: &Path) -> bool {
    liveness_in(dir, &procscan::scan()) == Liveness::Idle
}

/// `argv[0]` may go through `dir/bin` or the versioned dir it links to, so both spellings count.
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

fn liveness_of(mine: &[&ProcInfo]) -> Liveness {
    if mine.iter().any(|p| p.comm == WORKER_COMM) {
        Liveness::Busy
    } else if mine.iter().any(|p| p.comm == LISTENER_COMM) {
        Liveness::Idle
    } else {
        Liveness::Offline
    }
}

/// `.runner` files carry a UTF-8 BOM, which `serde_json` rejects.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn clock_ticks() -> u64 {
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
            RunnerScope::parse(&p.github_url).unwrap().login(),
            "example-org"
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
    fn scratch_is_temp_under_the_work_folder_and_diag_at_the_install_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".runner"),
            "{\"agentId\":1,\"agentName\":\"r0\",\
             \"gitHubUrl\":\"https://github.com/example-org\",\"workFolder\":\"work\"}",
        )
        .unwrap();
        let s = scratch_for(dir.path()).unwrap();
        assert_eq!(s.temp(), dir.path().join("work/_temp"));
        assert_eq!(s.diag(), dir.path().join("_diag"));
        assert_eq!(s.uid(), nix::unistd::geteuid().as_raw());
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
