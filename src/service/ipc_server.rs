//! The collector half of the IPC: a `UnixListener` on the scope's socket path,
//! answering the TUI's read-only queries from a WAL reader connection. Modeled
//! on `metrics::pull::spawn` — a named thread, its own reader connection, a
//! non-fatal bind, and a `term`-polled (non-blocking) accept loop so a SIGTERM
//! exits promptly. The handlers are thin adapters over `store::reader`.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;

use crate::service::store::{self, reader};
use crate::shared::config::Config;
use crate::shared::ipc::{self, ApiRow, Request, Response, VERSION};
use crate::shared::paths::Scope;

/// How often the non-blocking accept loop wakes to re-check the shutdown flag.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Per-connection I/O timeout — a wedged client can't stall the accept loop.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn the IPC server thread. Always spawns; a bind failure is logged and the
/// thread returns (the collector keeps sampling), exactly like `metrics::pull`.
pub fn spawn(cfg: &Config, term: Arc<AtomicBool>) -> JoinHandle<()> {
    // Bind the socket for the process's own scope — the same scope `systemd
    // install` placed the DB + unit under (root ⇒ System ⇒ /run/ghr-stats).
    let sock = Scope::detect().socket_path();
    let db = cfg.db_path.clone();
    thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || run(&sock, &db, &term))
        .expect("spawn ipc-server")
}

fn run(sock: &Path, db: &Path, term: &AtomicBool) {
    let listener = match bind(sock) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, sock = %sock.display(),
                "ipc: bind failed — Persistent-mode TUI features unavailable");
            return;
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        tracing::error!(error = %e, "ipc: set_nonblocking failed");
        return;
    }
    tracing::info!(sock = %sock.display(), "ipc listening");

    let conn = store::open_reader(db);
    while !term.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // One local client at a low rate — serve inline. A slow client is
                // bounded by CONN_TIMEOUT, so it can't wedge the accept loop.
                if let Err(e) = serve_conn(stream, conn.as_ref()) {
                    tracing::debug!(error = %e, "ipc: connection ended");
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(ACCEPT_POLL),
            Err(e) => {
                tracing::warn!(error = %e, "ipc: accept");
                thread::sleep(ACCEPT_POLL);
            }
        }
    }
    // Best-effort: systemd's RuntimeDirectory= also removes this on stop.
    let _ = std::fs::remove_file(sock);
    tracing::debug!("ipc stopped");
}

/// Create the runtime dir, clear any stale socket, bind, and widen perms so a
/// non-root TUI can connect.
fn bind(sock: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A stale socket (unclean prior exit) makes bind fail EADDRINUSE. Removing it
    // is safe: serve holds the exclusive flock before spawning us, so no live
    // collector owns this path.
    if sock.exists() {
        let _ = std::fs::remove_file(sock);
    }
    let listener = UnixListener::bind(sock)?;
    // connect(2) needs WRITE permission on the socket file; bind creates it
    // ~0755 (umask), which a non-root TUI cannot connect to. Widen to 0666 — the
    // same unauthenticated-loopback posture as /metrics; the IPC serves only
    // derived fleet stats, never tokens. The parent dir is root-only-writable, so
    // this is not a meaningful TOCTOU.
    std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Serve requests on one connection until the client hangs up (EOF) or errors.
fn serve_conn(mut stream: UnixStream, conn: Option<&Connection>) -> io::Result<()> {
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    loop {
        let req: Request = match ipc::read_frame(&mut stream) {
            Ok(r) => r,
            // Clean client hang-up between requests.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        ipc::write_frame(&mut stream, &handle(&req, conn))?;
    }
}

/// Map one request to a response via `store::reader`. A DB/query error becomes
/// `Response::Error` (logged client-side) rather than dropping the connection.
fn handle(req: &Request, conn: Option<&Connection>) -> Response {
    if let Request::Hello { .. } = req {
        return Response::Hello { server: VERSION };
    }
    let Some(conn) = conn else {
        return Response::Error("db unavailable".to_string());
    };
    match req {
        Request::Hello { .. } => unreachable!("handled above"),
        Request::HostSeries { limit } => {
            wrap(reader::host_series(conn, *limit), Response::HostSeries)
        }
        Request::BusySeries { limit } => {
            wrap(reader::busy_series(conn, *limit), Response::BusySeries)
        }
        Request::RunnerHistory { agent_id, limit } => wrap(
            reader::runner_history(conn, *agent_id, *limit),
            Response::RunnerHistory,
        ),
        Request::RecentJobs { limit } => {
            wrap(reader::recent_jobs(conn, *limit), Response::RecentJobs)
        }
        Request::ActiveJob { runner_name } => {
            wrap(reader::active_job(conn, runner_name), Response::ActiveJob)
        }
        Request::LatestApiRunners => wrap(reader::latest_api_runners(conn), |m| {
            Response::LatestApiRunners(
                m.into_iter()
                    .map(|(agent_id, state)| ApiRow { agent_id, state })
                    .collect(),
            )
        }),
        Request::RunnerStates => wrap(reader::runner_states(conn), |m| {
            Response::RunnerStates(m.into_values().collect())
        }),
    }
}

/// Fold a reader `Result<T>` into a `Response`: `ok` on success, `Error` on failure.
fn wrap<T>(res: crate::shared::error::Result<T>, ok: impl FnOnce(T) -> Response) -> Response {
    match res {
        Ok(v) => ok(v),
        Err(e) => Response::Error(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        store::schema_for_test(&mut conn);
        conn.execute(
            "INSERT INTO host_sample (ts, load1, load5, mem_used, mem_total) \
             VALUES (100, 1.0, 1.0, 10, 20)",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn hello_replies_with_server_version_without_a_db() {
        assert!(matches!(
            handle(&Request::Hello { client: VERSION }, None),
            Response::Hello { server } if server == VERSION
        ));
    }

    #[test]
    fn data_request_without_db_is_an_error_not_a_panic() {
        assert!(matches!(
            handle(&Request::HostSeries { limit: 5 }, None),
            Response::Error(_)
        ));
    }

    #[test]
    fn host_series_request_returns_rows() {
        let conn = seeded();
        assert!(matches!(
            handle(&Request::HostSeries { limit: 5 }, Some(&conn)),
            Response::HostSeries(v) if v.len() == 1 && v[0].ts == 100
        ));
    }

    #[test]
    fn latest_api_runners_serializes_as_pairs() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (200, 9, 'o', 'r', 1, 0)",
            [],
        )
        .unwrap();
        match handle(&Request::LatestApiRunners, Some(&conn)) {
            Response::LatestApiRunners(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].agent_id, 9);
                assert!(rows[0].state.online && !rows[0].state.busy);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn runner_states_returns_persisted_edges() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO runner_state (agent_id, liveness, since_ts, last_seen_ts) \
             VALUES (7, 'busy', 500, 900)",
            [],
        )
        .unwrap();
        match handle(&Request::RunnerStates, Some(&conn)) {
            Response::RunnerStates(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].agent_id, 7);
                assert_eq!(rows[0].since_ts, 500);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
