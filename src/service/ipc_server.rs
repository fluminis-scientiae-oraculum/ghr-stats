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

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use rusqlite::Connection;

use crate::service::store::{self, reader};
use crate::shared::config::{Config, SharedConfig, persist};
use crate::shared::ipc::{self, ApiRow, Mutation, Query, Request, Response, VERSION};
use crate::shared::paths::{self, ADMIN_GROUP, Scope};

/// How often the non-blocking accept loop wakes to re-check the shutdown flag.
const ACCEPT_POLL: Duration = Duration::from_millis(500);
/// Per-connection I/O timeout — a wedged client can't stall the accept loop.
const CONN_TIMEOUT: Duration = Duration::from_secs(5);

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
pub fn spawn(shared: &SharedConfig, term: Arc<AtomicBool>) -> JoinHandle<()> {
    // Bind the socket for the process's own scope — the same scope `systemd
    // install` placed the DB + unit under (root ⇒ System ⇒ /run/ghr-stats). The
    // DB path is fixed for the run, so snapshot it once here.
    let sock = Scope::detect().socket_path();
    let db = shared.snapshot().db_path.clone();
    let shared = shared.clone();
    thread::Builder::new()
        .name("ipc-server".into())
        .spawn(move || run(&sock, &db, &shared, &term))
        .expect("spawn ipc-server")
}

fn run(sock: &Path, db: &Path, shared: &SharedConfig, term: &AtomicBool) {
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
    // Authorized mutations write the canonical system config (/etc).
    let config_path = paths::config_write_target(None);
    while !term.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // One local client at a low rate — serve inline. A slow client is
                // bounded by CONN_TIMEOUT, so it can't wedge the accept loop.
                if let Err(e) = serve_conn(stream, conn.as_ref(), &config_path, shared) {
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
/// A successful mutation triggers an in-process config reload, so a change (e.g.
/// a newly added PAT) reaches the sampler/reconcile threads without a restart.
fn serve_conn(
    mut stream: UnixStream,
    conn: Option<&Connection>,
    config_path: &Path,
    shared: &SharedConfig,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CONN_TIMEOUT))?;
    stream.set_write_timeout(Some(CONN_TIMEOUT))?;
    // Resolve the peer's identity once, from the kernel — used to gate mutations.
    let auth = peer_auth(&stream);
    loop {
        let req: Request = match ipc::read_frame(&mut stream) {
            Ok(r) => r,
            // Clean client hang-up between requests.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let resp = handle(&req, conn, auth, config_path);
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
fn handle(req: &Request, conn: Option<&Connection>, auth: Auth, config_path: &Path) -> Response {
    match req {
        Request::Hello { .. } => Response::Hello { server: VERSION },
        // Reads: never authorized (derived stats + config presence, no secrets).
        Request::Query(q) => serve_query(q, conn, config_path),
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

/// Serve a read query. Exhaustive over [`Query`] (a new read variant is a compile
/// error until handled here — no `unreachable!`). The DB-availability check is
/// factored into [`with_db`], so only the arms that need the reader carry it;
/// `ConfiguredTokenOrgs` reads the config file instead.
fn serve_query(q: &Query, conn: Option<&Connection>, config_path: &Path) -> Response {
    match q {
        // Presence-only view of configured token orgs (config file, not the DB).
        Query::ConfiguredTokenOrgs => {
            Response::ConfiguredTokenOrgs(configured_token_orgs(config_path))
        }
        Query::HostSeries { limit } => {
            with_db(conn, |c| wrap(reader::host_series(c, *limit), Response::HostSeries))
        }
        Query::BusySeries { limit } => {
            with_db(conn, |c| wrap(reader::busy_series(c, *limit), Response::BusySeries))
        }
        Query::RunnerHistory { agent_id, limit } => with_db(conn, |c| {
            wrap(
                reader::runner_history(c, *agent_id, *limit),
                Response::RunnerHistory,
            )
        }),
        Query::RecentJobs { limit } => {
            with_db(conn, |c| wrap(reader::recent_jobs(c, *limit), Response::RecentJobs))
        }
        Query::LatestJob { runner_name } => with_db(conn, |c| {
            wrap(reader::latest_job(c, runner_name), Response::LatestJob)
        }),
        Query::LatestApiRunners => with_db(conn, |c| {
            wrap(reader::latest_api_runners(c), |m| {
                Response::LatestApiRunners(
                    m.into_iter()
                        .map(|(agent_id, state)| ApiRow { agent_id, state })
                        .collect(),
                )
            })
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
            tracing::info!(peer_uid = auth.uid, action = m.action(), "ipc: config mutated");
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
    const ROOT: Auth = Auth { uid: 0, in_admin_group: false };
    const MEMBER: Auth = Auth { uid: 1000, in_admin_group: true };
    const NOBODY: Auth = Auth { uid: 1000, in_admin_group: false };
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
            handle(&Request::Hello { client: VERSION }, None, NOBODY, &noconf()),
            Response::Hello { server } if server == VERSION
        ));
    }

    #[test]
    fn data_request_without_db_is_an_error_not_a_panic() {
        assert!(matches!(
            handle(&Request::Query(Query::HostSeries { limit: 5 }), None, ROOT, &noconf()),
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
                &noconf()
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
        match handle(&Request::Query(Query::LatestApiRunners), Some(&conn), ROOT, &noconf()) {
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
        match handle(&Request::Query(Query::RunnerStates), Some(&conn), ROOT, &noconf()) {
            Response::RunnerStates(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].agent_id, 7);
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
        match handle(&Request::Query(Query::ConfiguredTokenOrgs), None, NOBODY, &cfg) {
            Response::ConfiguredTokenOrgs(orgs) => {
                assert_eq!(orgs, vec!["acme".to_string(), "widgets".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }
        // A missing/unreadable config yields an empty list, never an error.
        assert!(matches!(
            handle(&Request::Query(Query::ConfiguredTokenOrgs), None, NOBODY, &noconf()),
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
            handle(&req, None, NOBODY, &cfg),
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
        assert!(matches!(handle(&req, None, MEMBER, &cfg), Response::Mutated));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("9999"), "persisted config should hold the new addr");
    }

    // NB: there is deliberately NO per-mutation "is it gated?" test. The
    // `Request::Mutate` branch is the sole path to `apply_mutation`, so the
    // single `mutation_denied_*` case above proves the gate for EVERY present
    // and future mutation — the structure guarantees it, not a test per variant.
}
