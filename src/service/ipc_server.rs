//! The collector half of the IPC: a `UnixListener` on the scope's socket path,
//! answering read-only queries from a WAL reader connection. Modeled on
//! `metrics::pull::spawn` — a named thread, a non-fatal bind, and a
//! `term`-polled (non-blocking) accept loop so a SIGTERM exits promptly. The
//! handlers are thin adapters over `store::reader`.
//!
//! Each accepted connection is served on **its own thread**, with its own reader
//! connection. Serving inline instead is what made a live dashboard lock every
//! other client out: `serve_conn` loops until its read times out, and a TUI that
//! refreshes inside `CONN_TIMEOUT` — which is the healthy case, by design —
//! never times out, so `accept` was never reached again.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use rusqlite::Connection;

use crate::service::store::{self, reader};
use crate::shared::config::{Config, SharedConfig, persist};
use crate::shared::ipc::{self, ApiRow, Mutation, Query, Request, Response, VERSION};
use crate::shared::paths::{ADMIN_GROUP, Scope};

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

/// The authenticated peer of a connection, from `SO_PEERCRED` (kernel-provided,
/// unspoofable). Resolved once per connection.
#[derive(Clone, Copy)]
struct Auth {
    uid: u32,
    in_admin_group: bool,
}

/// Whether a peer may mutate config: root, or a member of [`ADMIN_GROUP`]. Pure.
fn authorized(uid: u32, in_admin_group: bool) -> bool {
    uid == 0 || in_admin_group
}

/// Read the connection's peer credentials and resolve group membership. Fails
/// CLOSED — an unreadable peer is treated as unprivileged, never authorized.
fn peer_auth(stream: &UnixStream) -> Auth {
    match getsockopt(stream, PeerCredentials) {
        Ok(cred) => {
            let uid = cred.uid();
            Auth {
                uid,
                in_admin_group: uid_in_group(uid, ADMIN_GROUP),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "ipc: peer credentials unavailable — treating as unprivileged");
            Auth {
                uid: u32::MAX,
                in_admin_group: false,
            }
        }
    }
}

/// Whether `uid`'s group memberships (resolved from the group DB, so `usermod
/// -aG` takes effect without a re-login) include `group`.
fn uid_in_group(uid: u32, group: &str) -> bool {
    let Some(user) = uzers::get_user_by_uid(uid) else {
        return false;
    };
    uzers::get_user_groups(user.name(), user.primary_group_id())
        .into_iter()
        .flatten()
        .any(|g| g.name().to_str() == Some(group))
}

/// Spawn the IPC server thread. Always spawns; a bind failure is logged and the
/// thread returns (the collector keeps sampling), exactly like `metrics::pull`.
/// Holds the [`SharedConfig`] so it can reload the collector's config in-process
/// after an authorized mutation (making a newly added PAT live without restart).
pub fn spawn(shared: &SharedConfig, term: Arc<AtomicBool>, config_path: PathBuf) -> JoinHandle<()> {
    // Bind the socket for the process's own scope — the same scope `systemd
    // install` placed the DB + unit under (root ⇒ System ⇒ /run/ghr-stats). The
    // DB path is fixed for the run, so snapshot it once here.
    let sock = Scope::detect().socket_path();
    let db = shared.snapshot().db_path.clone();
    let shared = shared.clone();
    thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || run(&sock, &db, &shared, &term, &config_path))
        .expect("spawn ipc-server")
}

fn run(sock: &Path, db: &Path, shared: &SharedConfig, term: &Arc<AtomicBool>, config_path: &Path) {
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

/// Map one request to a response. Reads go through `store::reader`; mutations go
/// through the authz gate to `config::persist` (writing `config_path`). A DB or
/// query error becomes `Response::Error` rather than dropping the connection.
fn handle(
    req: &Request,
    conn: Option<&Connection>,
    auth: Auth,
    config_path: &Path,
    max_age: u64,
) -> Response {
    match req {
        Request::Hello { .. } => Response::Hello {
            server: VERSION,
            // Report our BUILD version too, so a TUI can tell "the service is
            // an older binary" from "no service" — the upgrade-without-restart
            // case, which otherwise looks identical to an absent collector.
            version: crate::shared::util::BUILD_VERSION.to_string(),
        },
        // Reads: never authorized (derived stats + config presence, no secrets).
        Request::Query(q) => serve_query(q, conn, config_path, max_age),
        // Writes: the ONE authz gate. `apply_mutation` is reachable only past it,
        // so no mutation — present or future — can skip authorization.
        Request::Mutate(m) => {
            if !authorized(auth.uid, auth.in_admin_group) {
                tracing::warn!(
                    peer_uid = auth.uid,
                    action = m.action(),
                    "ipc: config mutation denied (need root or the ghr-stats group)"
                );
                return Response::Denied;
            }
            apply_mutation(m, auth, config_path)
        }
    }
}

/// Upper bound on any read query's `limit`. The IPC is unauthenticated by design
/// and reachable by any local user (the socket is 0666), so an unclamped `limit`
/// — `usize::MAX` casts to a negative `i64`, which SQLite treats as "no limit" —
/// would force a full-table scan + full JSON serialize. Cap it far above any real
/// history/trend window.
const MAX_QUERY_LIMIT: usize = 10_000;

/// Clamp a client-supplied query limit to [`MAX_QUERY_LIMIT`]. Pure + tested.
fn clamped(limit: usize) -> usize {
    limit.min(MAX_QUERY_LIMIT)
}

/// `timeline`'s own, tighter bound.
///
/// Its rows are an order of magnitude wider than a `HistPoint` — org, runner
/// name and an adjudicated GitHub view per sample — so `MAX_QUERY_LIMIT` rows
/// would serialize past `MAX_FRAME` and fail the whole reply rather than
/// returning a short one. A cap that turns a large request into a *bounded
/// answer* is the point of the verb; a cap that turns it into an error is not.
/// Sized so even the widest row shape stays comfortably inside the frame.
const MAX_TIMELINE_LIMIT: usize = 2_000;

/// Serve a read query. Exhaustive over [`Query`] (a new read variant is a compile
/// error until handled here — no `unreachable!`). The DB-availability check is
/// factored into [`with_db`], so only the arms that need the reader carry it;
/// `ConfiguredTokenOrgs` reads the config file instead. Every `limit` is clamped
/// to [`MAX_QUERY_LIMIT`] before it reaches the reader.
fn serve_query(q: &Query, conn: Option<&Connection>, config_path: &Path, max_age: u64) -> Response {
    match q {
        // Presence-only view of configured token orgs (config file, not the DB).
        Query::ConfiguredTokenOrgs => {
            Response::ConfiguredTokenOrgs(configured_token_orgs(config_path))
        }
        Query::HostSeries { limit } => with_db(conn, |c| {
            wrap(
                reader::host_series(c, clamped(*limit)),
                Response::HostSeries,
            )
        }),
        Query::BusySeries { limit } => with_db(conn, |c| {
            wrap(
                reader::busy_series(c, clamped(*limit)),
                Response::BusySeries,
            )
        }),
        Query::RunnerHistory { dir, limit } => with_db(conn, |c| {
            wrap(
                reader::runner_history(c, dir, clamped(*limit)),
                Response::RunnerHistory,
            )
        }),
        Query::RecentJobs { limit } => with_db(conn, |c| {
            wrap(
                reader::recent_jobs(c, clamped(*limit)),
                Response::RecentJobs,
            )
        }),
        Query::LatestJob { runner_name } => with_db(conn, |c| {
            wrap(reader::latest_job(c, runner_name), Response::LatestJob)
        }),
        Query::LatestApiRunners => with_db(conn, |c| {
            wrap(
                reader::latest_api_runners(c, crate::shared::util::now_epoch(), max_age),
                |m| {
                    Response::LatestApiRunners(
                        m.into_iter()
                            .map(|((org, agent_id), view)| ApiRow {
                                agent_id,
                                org,
                                view,
                            })
                            .collect(),
                    )
                },
            )
        }),
        // The verdict is computed collector-side, from the same Snapshot the
        // exporter uses — so `status`, /metrics and the push sink can never
        // disagree about whether the fleet is healthy.
        Query::FleetStatus => with_db(conn, |c| {
            wrap(
                crate::service::metrics::Snapshot::gather(
                    c,
                    crate::shared::util::now_epoch(),
                    crate::shared::util::BUILD_VERSION,
                    max_age,
                )
                .map(|s| Box::new(s.to_status(crate::shared::models::Mode::Persistent))),
                Response::FleetStatus,
            )
        }),
        // The window is the client's to choose; the ROW COUNT is not. Clamping
        // here rather than trusting the CLI's own cap is what keeps the bound
        // real — the socket is reachable by any local user, and `timeline` is
        // the first query whose natural answer is unbounded.
        Query::Timeline(q) => with_db(conn, |c| {
            let mut q = q.clone();
            q.limit = q.limit.min(MAX_TIMELINE_LIMIT);
            wrap(
                reader::timeline(c, &q, crate::shared::util::now_epoch(), max_age).map(Box::new),
                Response::Timeline,
            )
        }),
        Query::RunnerStates => with_db(conn, |c| {
            wrap(reader::runner_states(c), |m| {
                Response::RunnerStates(m.into_values().collect())
            })
        }),
    }
}

/// Apply an authorized config mutation. Reachable ONLY past the authz gate in
/// [`handle`]. Exhaustive over [`Mutation`] (a new write variant is a compile
/// error until handled — and it is automatically gated, since this is the only
/// caller). Success is audit-logged with the peer uid; a persist error becomes
/// `Response::Error`.
fn apply_mutation(m: &Mutation, auth: Auth, config_path: &Path) -> Response {
    let result = match m {
        Mutation::SetMetricsPull { enabled, addr } => {
            persist::set_metrics_pull(config_path, *enabled, addr)
        }
        Mutation::AddOrgToken { org, token } => persist::set_org_token(config_path, org, token),
        Mutation::RemoveOrgToken { org } => persist::remove_org_token(config_path, org),
    };
    match result {
        Ok(()) => {
            tracing::info!(
                peer_uid = auth.uid,
                action = m.action(),
                "ipc: config mutated"
            );
            Response::Mutated
        }
        Err(e) => Response::Error(e.to_string()),
    }
}

/// The configured token org logins, read FRESH from the system config (so a
/// just-persisted `[a]` addition is reflected without a collector restart) —
/// presence only, never a token value. An unreadable/malformed config ⇒ empty.
fn configured_token_orgs(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path)
        .map(|text| crate::shared::config::token_orgs(&text))
        .unwrap_or_default()
}

/// Run `f` with the DB reader connection, or reply `Error` if the DB is
/// unavailable — the single home for that check, so every read arm shares it.
fn with_db(conn: Option<&Connection>, f: impl FnOnce(&Connection) -> Response) -> Response {
    match conn {
        Some(c) => f(c),
        None => Response::Error("db unavailable".to_string()),
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

    use std::path::PathBuf;

    fn handshake(stream: &mut UnixStream) -> io::Result<Response> {
        ipc::write_frame(stream, &Request::Hello { client: VERSION })?;
        ipc::read_frame(stream)
    }

    /// A client that holds its connection open must not lock every other client
    /// out.
    ///
    /// The production failure this guards, found on the live fleet host: served
    /// inline, `serve_conn` runs until its client hangs up, and the dashboard
    /// holds its connection open for as long as it is on screen. One open TUI
    /// therefore owned the accept loop, every other client sat unaccepted in the
    /// kernel backlog, and `status` / `explain` timed out during the handshake
    /// and reported "no collector" — while the collector was healthy, listening,
    /// and answering the dashboard the whole time.
    ///
    /// The 2 s budget is deliberately well inside `CONN_TIMEOUT` (5 s). Served
    /// inline, the second client could not be accepted until the FIRST client's
    /// read timed out, so "is it served promptly" is exactly what separates the
    /// two designs — "is it served eventually" does not.
    #[test]
    fn a_held_connection_does_not_lock_out_a_second_client() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("serve.sock");
        let db = dir.path().join("history.db");
        let config_path = dir.path().join("config.toml");
        let term = Arc::new(AtomicBool::new(false));

        let server = {
            let (sock, db, config_path, term) = (
                sock.clone(),
                db.clone(),
                config_path.clone(),
                Arc::clone(&term),
            );
            thread::spawn(move || {
                let shared = SharedConfig::new(Config::default());
                run(&sock, &db, &shared, &term, &config_path);
            })
        };

        let mut first = (0..100)
            .find_map(|_| {
                UnixStream::connect(&sock).ok().or_else(|| {
                    thread::sleep(Duration::from_millis(20));
                    None
                })
            })
            .expect("collector never bound its socket");
        assert!(matches!(
            handshake(&mut first).expect("first handshake"),
            Response::Hello { .. }
        ));

        // `first` stays open and idle from here — precisely what an on-screen
        // dashboard does between refreshes, and precisely what used to wedge us.
        let mut second = UnixStream::connect(&sock).expect("second client could not connect");
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        match handshake(&mut second) {
            Ok(Response::Hello { .. }) => {}
            other => panic!(
                "a second client was not served while the first held its connection: {other:?}"
            ),
        }

        term.store(true, Ordering::SeqCst);
        drop(first);
        drop(second);
        server.join().unwrap();
    }

    /// `timeline`'s bound has to hold against the CLIENT, not just the CLI: the
    /// socket is reachable by any local user, so a caller that ignores its own
    /// cap must still get a bounded answer rather than an oversized frame the
    /// server then fails to send.
    #[test]
    fn a_timeline_limit_is_clamped_server_side() {
        let mut conn = Connection::open_in_memory().unwrap();
        store::schema_for_test(&mut conn);
        // Two samples, one edge — enough to prove the query ran, while the limit
        // being clamped is what the assertion is really about.
        for (ts, live) in [(100, "idle"), (200, "busy")] {
            conn.execute(
                "INSERT INTO runner_sample (ts, agent_id, name, org, liveness, dir) \
                 VALUES (?1, 1, 'r1', 'o', ?2, '/d1')",
                rusqlite::params![ts, live],
            )
            .unwrap();
        }
        let reply = handle(
            &Request::Query(Query::Timeline(
                crate::shared::models::timeline::TimelineQuery {
                    since_ts: 0,
                    // A limit that would serialize past MAX_FRAME if honoured.
                    limit: usize::MAX,
                    org: None,
                    runner: None,
                    samples: true,
                },
            )),
            Some(&conn),
            NOBODY,
            &noconf(),
            MAX_AGE,
        );
        match reply {
            Response::Timeline(t) => {
                assert_eq!(t.transitions.items.len(), 1);
                // Clamped, so the reply is a bounded answer — not the frame-too-
                // large error an unclamped `usize::MAX` would have produced.
                assert!(!t.transitions.limited);
                assert_eq!(t.samples.map(|s| s.items.len()), Some(2));
            }
            other => panic!("expected a timeline, got {other:?}"),
        }
    }

    #[test]
    fn query_limit_is_clamped() {
        // A hostile/huge limit is bounded; `usize::MAX` (which would cast to a
        // negative i64 = SQLite "no limit") is capped, not passed through.
        assert_eq!(clamped(5), 5);
        assert_eq!(clamped(MAX_QUERY_LIMIT), MAX_QUERY_LIMIT);
        assert_eq!(clamped(MAX_QUERY_LIMIT + 1), MAX_QUERY_LIMIT);
        assert_eq!(clamped(usize::MAX), MAX_QUERY_LIMIT);
    }

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

    // Auth fixtures + a config path reads never touch.
    const ROOT: Auth = Auth {
        uid: 0,
        in_admin_group: false,
    };
    const MEMBER: Auth = Auth {
        uid: 1000,
        in_admin_group: true,
    };
    const NOBODY: Auth = Auth {
        uid: 1000,
        in_admin_group: false,
    };
    /// Freshness window for tests whose assertions do not depend on it.
    use crate::shared::models::GhView;
    const MAX_AGE: u64 = 180;
    fn noconf() -> PathBuf {
        PathBuf::from("/nonexistent/ghr-stats-unused.toml")
    }

    #[test]
    fn authorized_only_for_root_or_group_member() {
        assert!(authorized(0, false)); // root
        assert!(authorized(1000, true)); // group member
        assert!(!authorized(1000, false)); // neither
    }

    #[test]
    fn hello_replies_with_server_version_without_a_db() {
        assert!(matches!(
            handle(&Request::Hello { client: VERSION }, None, NOBODY, &noconf(), MAX_AGE),
            Response::Hello { server, .. } if server == VERSION
        ));
    }

    #[test]
    fn data_request_without_db_is_an_error_not_a_panic() {
        assert!(matches!(
            handle(
                &Request::Query(Query::HostSeries { limit: 5 }),
                None,
                ROOT,
                &noconf(),
                MAX_AGE
            ),
            Response::Error(_)
        ));
    }

    #[test]
    fn host_series_request_returns_rows() {
        let conn = seeded();
        assert!(matches!(
            handle(
                &Request::Query(Query::HostSeries { limit: 5 }),
                Some(&conn),
                ROOT,
                &noconf(),
                MAX_AGE
            ),
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
        // A very wide window, so the row is unambiguously fresh regardless of
        // the wall clock this test runs at.
        match handle(
            &Request::Query(Query::LatestApiRunners),
            Some(&conn),
            ROOT,
            &noconf(),
            u64::MAX,
        ) {
            Response::LatestApiRunners(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].agent_id, 9);
                assert_eq!(rows[0].view.online(), Some(true));
                assert_eq!(rows[0].view.busy(), Some(false));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The wire must carry the freshness verdict, not a bare state the TUI
    /// would have to re-adjudicate. With a zero-second window the same row
    /// crosses as Stale, and its online/busy read as unknown rather than as a
    /// confident (and wrong) "online".
    #[test]
    fn latest_api_runners_reports_an_aged_row_as_stale_over_the_wire() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO api_runner_sample (ts, agent_id, org, name, online, busy) \
             VALUES (200, 9, 'o', 'r', 1, 0)",
            [],
        )
        .unwrap();
        match handle(
            &Request::Query(Query::LatestApiRunners),
            Some(&conn),
            ROOT,
            &noconf(),
            0,
        ) {
            Response::LatestApiRunners(rows) => {
                assert_eq!(rows.len(), 1);
                assert!(matches!(rows[0].view, GhView::Stale { .. }));
                assert_eq!(rows[0].view.online(), None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn runner_states_returns_persisted_edges() {
        let conn = seeded();
        conn.execute(
            "INSERT INTO runner_state (dir, liveness, since_ts, last_seen_ts) \
             VALUES ('/srv/r7', 'busy', 500, 900)",
            [],
        )
        .unwrap();
        match handle(
            &Request::Query(Query::RunnerStates),
            Some(&conn),
            ROOT,
            &noconf(),
            MAX_AGE,
        ) {
            Response::RunnerStates(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].dir, "/srv/r7");
                assert_eq!(rows[0].since_ts, 500);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn configured_token_orgs_reads_the_config_needs_no_auth_and_hides_values() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[github.tokens]\nwidgets = \"github_pat_SECRET\"\nacme = \"github_pat_OTHER\"\n",
        )
        .unwrap();
        // NOBODY (unauthorized for mutations) can still read presence — org logins
        // aren't secret. No DB needed.
        match handle(
            &Request::Query(Query::ConfiguredTokenOrgs),
            None,
            NOBODY,
            &cfg,
            MAX_AGE,
        ) {
            Response::ConfiguredTokenOrgs(orgs) => {
                assert_eq!(orgs, vec!["acme".to_string(), "widgets".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }
        // A missing/unreadable config yields an empty list, never an error.
        assert!(matches!(
            handle(&Request::Query(Query::ConfiguredTokenOrgs), None, NOBODY, &noconf(), MAX_AGE),
            Response::ConfiguredTokenOrgs(orgs) if orgs.is_empty()
        ));
    }

    #[test]
    fn mutation_denied_for_unauthorized_peer_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let req = Request::Mutate(Mutation::SetMetricsPull {
            enabled: true,
            addr: "127.0.0.1:9999".to_string(),
        });
        assert!(matches!(
            handle(&req, None, NOBODY, &cfg, MAX_AGE),
            Response::Denied
        ));
        assert!(!cfg.exists(), "denied mutation must not write the config");
    }

    #[test]
    fn mutation_persists_for_authorized_peer() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let req = Request::Mutate(Mutation::SetMetricsPull {
            enabled: true,
            addr: "127.0.0.1:9999".to_string(),
        });
        // A group member is authorized (as is root).
        assert!(matches!(
            handle(&req, None, MEMBER, &cfg, MAX_AGE),
            Response::Mutated
        ));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            text.contains("9999"),
            "persisted config should hold the new addr"
        );
    }

    // NB: there is deliberately NO per-mutation "is it gated?" test. The
    // `Request::Mutate` branch is the sole path to `apply_mutation`, so the
    // single `mutation_denied_*` case above proves the gate for EVERY present
    // and future mutation — the structure guarantees it, not a test per variant.
}
