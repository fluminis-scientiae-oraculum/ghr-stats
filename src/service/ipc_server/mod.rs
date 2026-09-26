//! The collector half of the IPC: a `UnixListener` on the scope's socket path,
//! answering read-only queries from a WAL reader connection. Modeled on
//! `metrics::pull::spawn` — a named thread, a non-fatal bind, and a
//! `term`-polled (non-blocking) accept loop so a SIGTERM exits promptly.
//!
//! This file owns a connection's LIFETIME — bind, accept, admit, serve, drop —
//! and nothing about what a request means. The two things it hands off are the
//! two questions it deliberately does not answer:
//!
//! - [`auth`] — *who is on the other end*, from the kernel rather than the wire.
//! - [`dispatch`] — *what the request means*, from the store and the config file.
//!
//! The split is not cosmetic: the three have never changed together. Fixing the
//! lockout below touched only this file, adding `Query::Retention` touched only
//! [`dispatch`], and the mutation authz model touched only [`auth`]. Their import
//! lists say the same thing — `nix`/`uzers` appear in one, `rusqlite`/`reader` in
//! another, `UnixListener`/`thread` in the third, and `Request`/`Response` in all
//! three because passing those across is the whole interface.
//!
//! Each accepted connection is served on **its own thread**, with its own reader
//! connection. Serving inline instead is what made a live dashboard lock every
//! other client out: `serve_conn` loops until its read times out, and a TUI that
//! refreshes inside `CONN_TIMEOUT` — which is the healthy case, by design —
//! never times out, so `accept` was never reached again.

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

/// How often the non-blocking accept loop wakes to re-check the shutdown flag.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Per-connection I/O timeout. Bounds how long a *silent* client holds a slot;
/// it is NOT what keeps the accept loop free — a healthy client never reaches it.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);
/// How many connections may be served at once.
///
/// The socket is `0666`, so this doubles as the bound on what a local user can
/// pin. Past it a connection is accepted and dropped immediately: refusing
/// visibly beats queueing behind an accept loop that will not come back, which
/// is the failure this cap replaces.
const MAX_CONNS: usize = 8;

/// The socket this process serves: its own scope's, the one `systemd install`
/// placed the unit under.
pub fn socket_path() -> PathBuf {
    Scope::detect().socket_path()
}

/// Spawn the IPC server thread on an already-bound `listener`. Holds the
/// [`SharedConfig`] so an authorized mutation reloads it in-process.
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
        // Reap finished workers so the vector stays bounded by MAX_CONNS rather
        // than by the number of clients this collector has ever served.
        workers.retain(|w| !w.is_finished());
        match listener.accept() {
            Ok((stream, _addr)) => {
                // A thread per connection, NOT inline. `serve_conn` runs until
                // its client hangs up, and a dashboard holds its connection open
                // for as long as it is on screen — inline, that one client owned
                // the accept loop and every other client queued behind it
                // forever.
                // Check-then-increment is sound without a CAS because this is the
                // only thread that ever increments; workers only ever decrement.
                if live.load(Ordering::SeqCst) >= MAX_CONNS {
                    tracing::warn!(max = MAX_CONNS, "ipc: at capacity, dropping connection");
                    continue; // `stream` closes here — the client sees a clean hang-up
                }
                live.fetch_add(1, Ordering::SeqCst);
                let slot = Slot(Arc::clone(&live));
                match spawn_conn(stream, db, shared, term, config_path, slot) {
                    Ok(w) => workers.push(w),
                    // The closure — and with it `slot` — is dropped here, so a
                    // failed spawn releases its slot without a manual decrement.
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
    // `term` is set, and every worker polls it between requests, so each returns
    // within one read timeout at worst. Join before removing the socket so no
    // worker is still answering on a path we have already unlinked.
    for w in workers {
        let _ = w.join();
    }
    // Best-effort: systemd's RuntimeDirectory= also removes this on stop.
    let _ = std::fs::remove_file(sock);
    tracing::debug!("ipc stopped");
}

/// Holds one of the [`MAX_CONNS`] connection slots, releasing it on drop.
///
/// A slot released by an explicit decrement at the end of the worker would leak
/// on any early return or panic, and a leaked slot is permanent — it shrinks the
/// server's capacity for the life of the process. Drop cannot be skipped.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve one connection on its own thread, with its own reader connection.
///
/// Per-thread rather than shared because `rusqlite::Connection` is not `Sync`,
/// and opening a WAL reader is cheap — the alternative, one connection behind a
/// mutex, would re-serialize exactly what this split exists to unserialize.
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
            let _slot = slot; // released when this thread ends, however it ends
            let conn = store::open_reader(&db);
            if let Err(e) = serve_conn(stream, conn.as_ref(), &config_path, &shared, &term) {
                tracing::debug!(error = %e, "ipc: connection ended");
            }
        })
}

/// Bind `sock`, replacing a stale socket but refusing one a live collector still
/// answers on (the serve lock is per database, the socket per scope). Widened to
/// 0666 so a non-root TUI can connect; the parent dir is root-only-writable.
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

/// Serve requests on one connection until the client hangs up (EOF), the read
/// times out (a stalled/idle client is dropped rather than held forever), or
/// shutdown is signalled. Polls `term` between requests so a pending SIGTERM
/// exits promptly instead of blocking on a connected client; the live TUI
/// refreshes well inside `CONN_TIMEOUT`, so it is never dropped mid-use, and it
/// reconnects on the next refresh if it ever is. A successful mutation triggers
/// an in-process config reload so a change (e.g. a newly added PAT) reaches the
/// sampler/reconcile threads without a restart.
fn serve_conn(
    mut stream: UnixStream,
    conn: Option<&Connection>,
    config_path: &Path,
    shared: &SharedConfig,
    term: &AtomicBool,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    // Resolve the peer's identity once, from the kernel — used to gate mutations.
    let auth = peer_auth(&stream);
    loop {
        if term.load(Ordering::SeqCst) {
            return Ok(()); // shutdown — release the connection so `serve` can exit
        }
        let req: Request = match ipc::read_frame(&mut stream) {
            Ok(r) => r,
            // Clean client hang-up between requests.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            // Read timed out: the client sent no complete frame within the window.
            // Drop it (freeing the accept loop) rather than hold the sole
            // connection open indefinitely — a would-be local DoS.
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
        // Snapshot per request, so a config mutation that widens the freshness
        // window takes effect immediately — same live-reload property the
        // sampler threads have.
        let max_age = shared.snapshot().intervals.api_max_age();
        let resp = handle(&req, conn, auth, config_path, max_age);
        // A persisted mutation just changed /etc — reload so the running workers
        // pick it up live (the whole point of the shared, swappable config).
        if matches!(resp, Response::Mutated) {
            shared.store(reload_config(config_path));
            tracing::info!("ipc: config reloaded after mutation");
        }
        ipc::write_frame(&mut stream, &resp)?;
    }
}

/// Reload the collector's config from disk, re-applying systemd root discovery
/// (as `serve` does at startup) so an empty `runner_roots` still finds the fleet.
/// An unreadable/invalid file falls back to defaults rather than failing.
fn reload_config(config_path: &Path) -> Config {
    let mut cfg = Config::load(Some(config_path)).unwrap_or_default();
    cfg.runner_roots = crate::shared::collectors::runners::effective_roots(&cfg.runner_roots);
    cfg
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
