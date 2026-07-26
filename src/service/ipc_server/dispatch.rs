//! What a request means.
//!
//! One request in, one response out, with no notion of sockets, threads or
//! shutdown — [`super`] owns those. Everything here is a pure-ish adapter over
//! two backing stores, and which one an arm reaches for is the file's internal
//! grain: reads go to `store::reader` (plus `metrics::Snapshot` for the verdict),
//! writes and token-org presence go to the config file through `config::persist`.
//!
//! Both dispatch tables are exhaustive with no `_` arm, so a new [`Query`] or
//! [`Mutation`] variant is a compile error until it is handled here. That is also
//! what makes the authz gate total: [`apply_mutation`] is reachable only through
//! the one check in [`handle`], so every present and future mutation is gated by
//! construction rather than by remembering to add a check.

use std::path::Path;

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::config::persist;
use crate::shared::ipc::{ApiRow, Mutation, Query, Request, Response, VERSION};

use super::auth::{Auth, authorized};

/// Map one request to a response. Reads go through `store::reader`; mutations go
/// through the authz gate to `config::persist` (writing `config_path`). A DB or
/// query error becomes `Response::Error` rather than dropping the connection.
pub(super) fn handle(
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
                reader::busy_series(c, clamped(*limit), max_age),
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
        // Deliberately takes no window: the answer is a property of the store,
        // so there is nothing for a caller to bound and nothing to clamp.
        Query::Retention => with_db(conn, |c| {
            wrap(reader::retention(c), |earliest_ts| Response::Retention {
                earliest_ts,
            })
        }),
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

    use crate::service::store;

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
