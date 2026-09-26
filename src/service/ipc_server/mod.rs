//! Collector side of the IPC: read-only queries and authorized config mutations on the
//! scope's socket. One thread and one WAL reader per connection: a TUI holds its connection
//! open, so serving inline would starve `accept`.

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;

use crate::service::store;
use crate::shared::config::{Config, SharedConfig};
use crate::shared::ipc::{self, Request, Response};
use crate::shared::paths::Scope;

mod auth;
mod dispatch;

use auth::peer_auth;
use dispatch::handle;

const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Drops a silent client; a busy one never reaches it.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);
/// The socket is `0666`, so this also bounds what any local user can pin. Excess
/// connections are dropped, not queued.
const MAX_CONNS: usize = 8;

pub fn socket_path() -> PathBuf {
    Scope::detect().socket_path()
}

pub fn spawn(
    listener: UnixListener,
    shared: &SharedConfig,
    term: Arc<AtomicBool>,
    config_path: PathBuf,
) -> JoinHandle<()> {
    let sock = socket_path();
    let db = shared.snapshot().db_path.clone();
    let shared = shared.clone();
    thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || run(listener, &sock, &db, &shared, &term, &config_path))
        .expect("spawn ipc-server")
}

fn run(
    listener: UnixListener,
    sock: &Path,
    db: &Path,
    shared: &SharedConfig,
    term: &Arc<AtomicBool>,
    config_path: &Path,
) {
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::error!(error = %e, "ipc: set_nonblocking failed");
        return;
    }
    tracing::info!(sock = %sock.display(), "ipc listening");

    let live = Arc::new(AtomicUsize::new(0));
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !term.load(Ordering::SeqCst) {
        workers.retain(|w| !w.is_finished());
        match listener.accept() {
            Ok((stream, _addr)) => {
                // Only this thread increments, so check-then-increment needs no CAS.
                if live.load(Ordering::SeqCst) >= MAX_CONNS {
                    tracing::warn!(max = MAX_CONNS, "ipc: at capacity, dropping connection");
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);
                let slot = Slot(Arc::clone(&live));
                match spawn_conn(stream, db, shared, term, config_path, slot) {
                    Ok(w) => workers.push(w),
                    // A failed spawn drops `slot` with the closure; no manual decrement.
                    Err(e) => tracing::warn!(error = %e, "ipc: spawn connection thread"),
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => {
                tracing::warn!(error = %e, "ipc: accept");
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
    // Join before unlinking so no worker still answers on a removed path.
    for w in workers {
        let _ = w.join();
    }
    // systemd's RuntimeDirectory= also removes this on stop.
    let _ = std::fs::remove_file(sock);
    tracing::debug!("ipc stopped");
}

/// One of the [`MAX_CONNS`] slots, released on drop so an early return or panic cannot leak it.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `rusqlite::Connection` is not `Sync`; one shared behind a mutex would re-serialize clients.
fn spawn_conn(
    stream: UnixStream,
    db: &Path,
    shared: &SharedConfig,
    term: &Arc<AtomicBool>,
    config_path: &Path,
    slot: Slot,
) -> io::Result<JoinHandle<()>> {
    let db = db.to_path_buf();
    let shared = shared.clone();
    let term = Arc::clone(term);
    let config_path = config_path.to_path_buf();
    thread::Builder::new()
        .name("ipc-conn".into())
        .spawn(move || {
            let _slot = slot;
            let conn = store::open_reader(&db);
            if let Err(e) = serve_conn(stream, conn.as_ref(), &config_path, &shared, &term) {
                tracing::debug!(error = %e, "ipc: connection ended");
            }
        })
}

/// Replaces a stale socket, refuses a live one (the serve lock is per database, the socket
/// per scope). Mode 0666 so a non-root TUI can connect; the parent dir is
/// root-only-writable.
pub fn bind(sock: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if UnixStream::connect(sock).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another collector is serving {}", sock.display()),
        ));
    }
    if std::fs::symlink_metadata(sock).is_ok_and(|m| m.file_type().is_socket()) {
        std::fs::remove_file(sock)?;
    }
    let listener = UnixListener::bind(sock)?;
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

fn serve_conn(
    mut stream: UnixStream,
    conn: Option<&Connection>,
    config_path: &Path,
    shared: &SharedConfig,
    term: &AtomicBool,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    let auth = peer_auth(&stream);
    loop {
        if term.load(Ordering::SeqCst) {
            return Ok(());
        }
        let req: Request = match ipc::read_frame(&mut stream) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        // Per request, so a mutation that widens the freshness window applies immediately.
        let max_age = shared.snapshot().intervals.api_max_age();
        let resp = handle(&req, conn, auth, config_path, max_age);
        if matches!(resp, Response::Mutated)
            && let Some(cfg) = reload_config(config_path)
        {
            shared.store(cfg);
            tracing::info!("ipc: config reloaded after mutation");
        }
        ipc::write_frame(&mut stream, &resp)?;
    }
}

/// Mirrors `serve` startup, which applies systemd root discovery to an empty `runner_roots`.
/// A file that no longer loads leaves the running config in place.
fn reload_config(config_path: &Path) -> Option<Config> {
    match Config::load(Some(config_path)) {
        Ok(mut cfg) => {
            cfg.runner_roots =
                crate::shared::collectors::runners::effective_roots(&cfg.runner_roots);
            Some(cfg)
        }
        Err(e) => {
            tracing::error!(error = %e, "config reload failed; keeping the running config");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_replaces_a_stale_socket_but_refuses_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("serve.sock");

        drop(UnixListener::bind(&sock).unwrap());
        let live = bind(&sock).expect("stale socket replaced");

        let err = bind(&sock).expect_err("live socket kept");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(UnixStream::connect(&sock).is_ok());
        drop(live);
    }
}
