//! cgroup v2 resource reads for a runner's systemd service slice.
//!
//! We never name the slice ourselves: we read `/proc/<pid>/cgroup` of the
//! runner's listener process to learn its cgroup path, then read the controller
//! files under `/sys/fs/cgroup`. All of these are world-readable, so this works
//! without privilege and without depending on the systemd unit name.

use std::path::PathBuf;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The absolute cgroup-fs directory for a pid, or `None` if not on cgroup v2.
pub fn dir_for_pid(pid: u32) -> Option<PathBuf> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = parse_unified_path(&content)?;
    Some(PathBuf::from(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// Extract the cgroup-v2 ("unified") path from `/proc/<pid>/cgroup` content.
/// The v2 line is `0::<path>`.
pub fn parse_unified_path(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.to_string())
}

/// Current memory usage in bytes (`memory.current`) for a cgroup dir.
pub fn memory_current(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Cumulative CPU usage in microseconds (`usage_usec` from `cpu.stat`).
pub fn cpu_usage_usec(dir: &std::path::Path) -> Option<u64> {
    let content = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
    parse_usage_usec(&content)
}

pub fn parse_usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_path_extracted() {
        let c = "0::/system.slice/actions.runner.example-org.runner-01.service\n";
        assert_eq!(
            parse_unified_path(c).as_deref(),
            Some("/system.slice/actions.runner.example-org.runner-01.service")
        );
    }

    #[test]
    fn unified_path_ignores_v1_lines() {
        let c = "12:pids:/foo\n0::/system.slice/x.service\n5:cpu:/bar\n";
        assert_eq!(
            parse_unified_path(c).as_deref(),
            Some("/system.slice/x.service")
        );
        assert_eq!(parse_unified_path("3:cpu:/only-v1"), None);
    }

    #[test]
    fn usage_usec_parsed() {
        let stat = "usage_usec 123456789\nuser_usec 100\nsystem_usec 200\n";
        assert_eq!(parse_usage_usec(stat), Some(123_456_789));
        assert_eq!(parse_usage_usec("nr_periods 0"), None);
    }
}
