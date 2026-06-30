//! Host-wide resource sampling: load average, memory, per-NUMA-node memory,
//! and disk (free space cheap each tick; `_work` dir sizes only when asked).

use std::path::{Path, PathBuf};

use sysinfo::System;
use walkdir::WalkDir;

use crate::model::{HostSample, NumaNode};

const NODE_ROOT: &str = "/sys/devices/system/node";

/// Sample the host. `work_dirs` is `Some` only on the slow cadence when we walk
/// the (potentially large) `_work` trees; otherwise `work_bytes` stays `None`.
pub fn sample(
    now_epoch: i64,
    runner_roots: &[PathBuf],
    work_dirs: Option<&[PathBuf]>,
) -> HostSample {
    let load = System::load_average();
    let mut sys = System::new();
    sys.refresh_memory();

    HostSample {
        ts: now_epoch,
        load1: load.one,
        load5: load.five,
        mem_used: sys.used_memory(),
        mem_total: sys.total_memory(),
        numa: read_numa(),
        work_bytes: work_dirs.map(|dirs| dirs.iter().map(|d| dir_size(d)).sum()),
        tmp_bytes: fs_used_bytes(Path::new("/tmp")),
        root_free: runner_roots.first().and_then(|p| fs_avail_bytes(p)),
    }
}

fn read_numa() -> Vec<NumaNode> {
    let Ok(entries) = std::fs::read_dir(NODE_ROOT) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(node) = name
            .to_str()
            .and_then(|s| s.strip_prefix("node"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(e.path().join("meminfo")) {
            nodes.push(NumaNode {
                node,
                mem_total: mem_kb_field(&content, "MemTotal:").unwrap_or(0),
                mem_free: mem_kb_field(&content, "MemFree:").unwrap_or(0),
            });
        }
    }
    nodes.sort_by_key(|n| n.node);
    nodes
}

/// Bytes for a `Node N <Key> <kB> kB` line (returned in bytes, not kB).
fn mem_kb_field(content: &str, key: &str) -> Option<u64> {
    content
        .lines()
        .find(|l| l.contains(key))
        .and_then(|l| {
            let mut rev = l.split_whitespace().rev();
            let _unit = rev.next()?; // "kB"
            rev.next()?.parse::<u64>().ok()
        })
        .map(|kb| kb * 1024)
}

fn fs_avail_bytes(path: &Path) -> Option<u64> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    Some(s.blocks_available() as u64 * s.fragment_size() as u64)
}

fn fs_used_bytes(path: &Path) -> Option<u64> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    let used_blocks = (s.blocks() as u64).saturating_sub(s.blocks_free() as u64);
    Some(used_blocks * s.fragment_size() as u64)
}

fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numa_meminfo_fields() {
        let c = "Node 0 MemTotal:       131072000 kB\n\
                 Node 0 MemFree:          1000000 kB\n\
                 Node 0 MemUsed:        130072000 kB\n";
        assert_eq!(mem_kb_field(c, "MemTotal:"), Some(131_072_000 * 1024));
        assert_eq!(mem_kb_field(c, "MemFree:"), Some(1_000_000 * 1024));
        assert_eq!(mem_kb_field(c, "Missing:"), None);
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"12345").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b"), b"678").unwrap();
        assert_eq!(dir_size(dir.path()), 8);
        // A non-existent path is simply zero, never a panic.
        assert_eq!(dir_size(Path::new("/nonexistent/ghr/path")), 0);
    }
}
