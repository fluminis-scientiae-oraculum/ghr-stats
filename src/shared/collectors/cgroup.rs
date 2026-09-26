//! cgroup v2 reads for a runner's service, located via its listener's `/proc/<pid>/cgroup`
//! (world-readable; needs no privilege or unit name).

use std::path::PathBuf;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

pub fn dir_for_pid(pid: u32) -> Option<PathBuf> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = parse_unified_path(&content)?;
    Some(PathBuf::from(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// The cgroup v2 line is `0::<path>`.
pub fn parse_unified_path(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.to_string())
}

pub fn memory_current(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(dir.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

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

/// `anon + shmem`: excludes the reclaimable page cache that `memory.current` also charges.
pub fn memory_working_set(dir: &std::path::Path) -> Option<u64> {
    match std::fs::read_to_string(dir.join("memory.stat")) {
        Ok(stat) => parse_working_set(&stat),
        Err(_) => memory_current(dir),
    }
}

pub fn parse_working_set(memory_stat: &str) -> Option<u64> {
    // Trailing space stops `anon ` matching `anon_thp`/`inactive_anon` and `shmem `
    // matching `shmem_thp`.
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
        let stat = "anon 157286400\n\
                    file 10855808000\n\
                    shmem 4194304\n\
                    anon_thp 0\n\
                    inactive_anon 1048576\n\
                    active_file 10000000000\n\
                    inactive_file 855808000\n";
        assert_eq!(parse_working_set(stat), Some(157_286_400 + 4_194_304));
        assert_eq!(parse_working_set("anon_thp 999\ninactive_anon 999"), None);
        assert_eq!(parse_working_set("anon 4096\n"), Some(4096));
    }

    #[test]
    fn working_set_falls_back_to_memory_current_without_stat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.current"), "4096\n").unwrap();
        assert_eq!(memory_working_set(dir.path()), Some(4096));
    }
}
