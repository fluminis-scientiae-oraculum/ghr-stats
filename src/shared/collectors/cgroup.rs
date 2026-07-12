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

/// Working-set memory in bytes: `anon + shmem` from `memory.stat`. This excludes
/// the reclaimable file-backed page cache that `memory.current` also charges to
/// the cgroup — on an idle box that cache is the last job's footprint and merely
/// *looks* like a per-runner ceiling. Falls back to `memory_current` if
/// `memory.stat` is unreadable.
pub fn memory_working_set(dir: &std::path::Path) -> Option<u64> {
    match std::fs::read_to_string(dir.join("memory.stat")) {
        Ok(stat) => parse_working_set(&stat),
        Err(_) => memory_current(dir),
    }
}

/// `anon + shmem` from a cgroup v2 `memory.stat` body. `anon` is required;
/// `shmem` defaults to 0 when absent.
pub fn parse_working_set(memory_stat: &str) -> Option<u64> {
    // The trailing space in each key is load-bearing: it stops `anon ` from
    // matching `anon_thp` / `inactive_anon`, and `shmem ` from matching
    // `shmem_thp`.
    let field = |key: &str| -> Option<u64> {
        memory_stat
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
    };
    let anon = field("anon ")?;
    let shmem = field("shmem ").unwrap_or(0);
    Some(anon + shmem)
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

    #[test]
    fn working_set_is_anon_plus_shmem() {
        // Representative idle-runner memory.stat: the working set is anon+shmem;
        // the large reclaimable file cache (active_file/inactive_file) is excluded.
        let stat = "anon 157286400\n\
                    file 10855808000\n\
                    shmem 4194304\n\
                    anon_thp 0\n\
                    inactive_anon 1048576\n\
                    active_file 10000000000\n\
                    inactive_file 855808000\n";
        assert_eq!(parse_working_set(stat), Some(157_286_400 + 4_194_304));
        // The trailing-space guard: anon_thp / inactive_anon must not be read as `anon`.
        assert_eq!(parse_working_set("anon_thp 999\ninactive_anon 999"), None);
        // shmem is optional → anon + 0.
        assert_eq!(parse_working_set("anon 4096\n"), Some(4096));
    }

    #[test]
    fn working_set_falls_back_to_memory_current_without_stat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.current"), "4096\n").unwrap();
        // No memory.stat present → fall back to the raw memory.current read.
        assert_eq!(memory_working_set(dir.path()), Some(4096));
    }
}
