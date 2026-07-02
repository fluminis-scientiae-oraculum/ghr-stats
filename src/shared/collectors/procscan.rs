//! Minimal, dependency-light process enumeration via `/proc`.
//!
//! We read `/proc` directly rather than via a process-listing crate because the
//! canonical short name lives in `/proc/<pid>/comm` (always the 15-char kernel
//! `comm`, never an exe path), the owner uid is just the `/proc/<pid>` dir
//! owner, and these are world-readable even for other users' processes. That
//! makes liveness detection work unprivileged and keeps the parse under test.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// One observed process. `comm` is the kernel short name (`/proc/<pid>/comm`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub uid: u32,
    pub comm: String,
    /// Field 22 of `/proc/<pid>/stat`: start time in clock ticks since boot.
    pub starttime_ticks: u64,
}

/// Enumerate all readable processes. Best-effort: unreadable entries are
/// skipped, never fatal.
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
            continue; // non-numeric /proc entry
        };
        if let Some(info) = read_proc(&entry.path(), pid) {
            out.push(info);
        }
    }
    out
}

fn read_proc(dir: &Path, pid: u32) -> Option<ProcInfo> {
    let uid = std::fs::metadata(dir).ok()?.uid();
    let comm = std::fs::read_to_string(dir.join("comm"))
        .ok()?
        .trim_end()
        .to_string();
    let starttime_ticks = std::fs::read_to_string(dir.join("stat"))
        .ok()
        .and_then(|s| parse_starttime(&s))
        .unwrap_or(0);
    Some(ProcInfo {
        pid,
        uid,
        comm,
        starttime_ticks,
    })
}

/// Parse field 22 (start time, in clock ticks) from a `/proc/<pid>/stat` line.
///
/// The `comm` field (2) is wrapped in parens and may itself contain spaces and
/// parens, so we anchor on the *last* `)` and count fields from there: after
/// it, field 3 (state) is index 0, making start time index 19.
pub fn parse_starttime(stat: &str) -> Option<u64> {
    let rparen = stat.rfind(')')?;
    let rest = stat.get(rparen + 1..)?.trim_start();
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Process age in seconds from its start ticks, the system boot time, and the
/// clock tick rate. Returns `None` if the inputs imply a negative age.
pub fn uptime_secs(now_epoch: i64, btime: i64, clk_tck: u64, starttime_ticks: u64) -> Option<u64> {
    if clk_tck == 0 {
        return None;
    }
    let started_epoch = btime + (starttime_ticks / clk_tck) as i64;
    let age = now_epoch - started_epoch;
    (age >= 0).then_some(age as u64)
}

/// Read `btime` (boot epoch seconds) from `/proc/stat`.
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
        // Synthetic stat line: comm = "(weird ) name)" with spaces + parens.
        // Fields after the last ')': state=S(3) ... starttime should be 4242.
        // index: 0:S 1:1 2:1 3:1 4:0 5:-1 6:0 7:0 8:0 9:0 10:0 11:0 12:0
        //        13:0 14:0 15:0 16:0 17:0 18:0 19:4242
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
        // boot at epoch 1000, clk_tck 100, started 5000 ticks => +50s => 1050.
        // now 1200 => age 150.
        assert_eq!(uptime_secs(1200, 1000, 100, 5000), Some(150));
        // negative age guarded
        assert_eq!(uptime_secs(1000, 1000, 100, 500_000), None);
        assert_eq!(uptime_secs(1200, 1000, 0, 5000), None);
    }

    #[test]
    fn btime_parsed() {
        let s = "cpu  1 2 3\nbtime 1700000000\nprocesses 42\n";
        assert_eq!(parse_btime(s), Some(1_700_000_000));
        assert_eq!(parse_btime("no btime here"), None);
    }
}
