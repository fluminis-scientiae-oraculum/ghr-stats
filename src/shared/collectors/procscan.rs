//! Process enumeration via `/proc`, whose `comm`, `cmdline` and `stat` are
//! world-readable unless `/proc` is mounted with `hidepid`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One observed process. `comm` is the kernel short name (`/proc/<pid>/comm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub comm: String,
    /// `argv[0]`, read only for runner processes (`comm` starting `Runner.`).
    pub argv0: Option<PathBuf>,
    /// Field 22 of `/proc/<pid>/stat`: start time in clock ticks since boot.
    pub starttime_ticks: u64,
}

pub fn scan() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(info) = read_proc(&entry.path(), pid) {
            out.push(info);
        }
    }
    out
}

fn read_proc(dir: &Path, pid: u32) -> Option<ProcInfo> {
    let comm = std::fs::read_to_string(dir.join("comm"))
        .ok()?
        .trim_end()
        .to_string();
    let argv0 = if comm.starts_with("Runner.") {
        std::fs::read(dir.join("cmdline"))
            .ok()
            .and_then(|b| parse_argv0(&b))
    } else {
        None
    };
    let starttime_ticks = std::fs::read_to_string(dir.join("stat"))
        .ok()
        .and_then(|s| parse_starttime(&s))
        .unwrap_or(0);
    Some(ProcInfo {
        pid,
        comm,
        argv0,
        starttime_ticks,
    })
}

fn parse_argv0(cmdline: &[u8]) -> Option<PathBuf> {
    let first = cmdline.split(|b| *b == 0).next()?;
    (!first.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(first)))
}

/// Field 22 (start time, clock ticks) of a `/proc/<pid>/stat` line. `comm` may contain spaces and
/// parens, so fields are counted from the last `)`: state is index 0, start time index 19.
pub fn parse_starttime(stat: &str) -> Option<u64> {
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 1..)?.trim_start();
    rest.split_whitespace().nth(19)?.parse().ok()
}

pub fn uptime_secs(now_epoch: i64, btime: i64, clk_tck: u64, starttime_ticks: u64) -> Option<u64> {
    if clk_tck == 0 {
        return None;
    }
    let started_epoch = btime + (starttime_ticks / clk_tck) as i64;
    let age = now_epoch - started_epoch;
    (age >= 0).then_some(age as u64)
}

/// `btime` (boot time, epoch seconds) from `/proc/stat`.
pub fn boot_time() -> Option<i64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    parse_btime(&stat)
}

pub fn parse_btime(proc_stat: &str) -> Option<i64> {
    proc_stat
        .lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_handles_comm_with_spaces_and_parens() {
        let line = "1234 (weird ) name) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 4242 99999";
        assert_eq!(parse_starttime(line), Some(4242));
    }

    #[test]
    fn starttime_simple() {
        let line = "451837 (Runner.Listener) S 451762 451762 451762 0 -1 \
                    4194304 0 0 0 0 10 5 0 0 20 0 30 0 8675309 0 0";
        assert_eq!(parse_starttime(line), Some(8675309));
    }

    #[test]
    fn uptime_computation() {
        assert_eq!(uptime_secs(1200, 1000, 100, 5000), Some(150));
        assert_eq!(uptime_secs(1000, 1000, 100, 500_000), None);
        assert_eq!(uptime_secs(1200, 1000, 0, 5000), None);
    }

    #[test]
    fn argv0_is_the_first_nul_separated_field() {
        assert_eq!(
            parse_argv0(b"/srv/r0/bin/Runner.Listener\0run\0--startuptype\0service\0"),
            Some(PathBuf::from("/srv/r0/bin/Runner.Listener"))
        );
        assert_eq!(parse_argv0(b""), None);
    }

    #[test]
    fn btime_parsed() {
        let s = "cpu  1 2 3\nbtime 1700000000\nprocesses 42\n";
        assert_eq!(parse_btime(s), Some(1_700_000_000));
        assert_eq!(parse_btime("no btime here"), None);
    }
}
