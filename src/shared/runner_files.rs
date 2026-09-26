//! Files inside a runner's install dir. The runner user owns that dir and its CI
//! jobs run as that user, so every read refuses symlinks, non-regular files, hard
//! links and foreign owners, and never reads past a cap.

use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use nix::fcntl::OFlag;

/// Owner, group and permission bits of a runner file, taken from the open handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ownership {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl Ownership {
    fn of(m: &Metadata) -> Self {
        Self {
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode() & 0o7777,
        }
    }

    /// What a file the runner user creates in `dir` gets.
    pub(crate) fn default_in(dir: &Path) -> io::Result<Self> {
        let m = std::fs::metadata(dir)?;
        Ok(Self {
            uid: m.uid(),
            gid: m.gid(),
            mode: 0o644,
        })
    }
}

/// Open `dir/name` for reading. `name` is a single path component.
pub(crate) fn open(dir: &Path, name: &str) -> io::Result<(File, Metadata)> {
    let owner = std::fs::metadata(dir)?.uid();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(dir.join(name))?;
    let m = file.metadata()?;
    if !m.file_type().is_file() {
        return Err(refused(name, "not a regular file"));
    }
    if m.nlink() != 1 {
        return Err(refused(name, "hard-linked"));
    }
    if m.uid() != owner && m.uid() != 0 {
        return Err(refused(name, "owned by neither the runner nor root"));
    }
    Ok((file, m))
}

/// Read `dir/name` as UTF-8, failing if it holds more than `cap` bytes.
pub(crate) fn read_text(dir: &Path, name: &str, cap: u64) -> io::Result<(String, Ownership)> {
    let (file, m) = open(dir, name)?;
    let mut buf = Vec::new();
    file.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(refused(name, "larger than expected"));
    }
    let text = String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{name}: not UTF-8")))?;
    Ok((text, Ownership::of(&m)))
}

fn refused(name: &str, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{name}: refused, {why}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plain_file_with_its_ownership() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        let (text, own) = read_text(dir.path(), ".env", 64).unwrap();
        assert_eq!(text, "A=1\n");
        assert_eq!(own.uid, nix::unistd::geteuid().as_raw());
    }

    #[test]
    fn refuses_symlink_fifo_hardlink_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("target"), "secret").unwrap();
        std::os::unix::fs::symlink(d.join("target"), d.join("link")).unwrap();
        nix::unistd::mkfifo(&d.join("fifo"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        std::fs::hard_link(d.join("target"), d.join("hard")).unwrap();
        std::fs::write(d.join("big"), vec![b'x'; 65]).unwrap();

        for name in ["link", "fifo", "hard", "big"] {
            assert!(read_text(d, name, 64).is_err(), "{name} was read");
        }
    }
}
