//! Request to response. Neither dispatch table has a `_` arm, and [`apply_mutation`]
//! must stay reachable only past the authz check in [`handle`].

use std::path::Path;

use rusqlite::Connection;

use crate::service::store::reader;
use crate::shared::config::persist;
use crate::shared::ipc::{ApiRow, Mutation, Query, Request, Response, VERSION};

use super::auth::{Admin, Peer};

pub(super) fn handle(
    req: &Request,
    conn: Option<&Connection>,
    peer: &Peer,
    config_path: &Path,
    max_age: u64,
) -> Response {
    match req {
        Request::Hello { .. } => Response::Hello {
            server: VERSION,
            // Lets a TUI tell an older running binary from an absent collector.
            version: crate::shared::util::BUILD_VERSION.to_string(),
        },
        // Reads are ungated: derived stats and config presence, no secrets.
        Request::Query(q) => serve_query(q, conn, config_path, max_age),
        Request::Mutate(m) => match peer.admin() {
            Some(admin) => apply_mutation(m, admin, config_path),
            None => {
                tracing::warn!(
                    peer_uid = ?peer.uid(),
                    action = m.action(),
                    "ipc: config mutation denied (need root or the ghr-stats group)"
                );
                Response::Denied
            }
        },
    }
}

/// The socket is reachable by any local user, and `usize::MAX` casts to a negative
/// `i64`, which SQLite treats as no limit.
const MAX_QUERY_LIMIT: usize = 10_000;

fn clamped(limit: usize) -> usize {
    limit.min(MAX_QUERY_LIMIT)
}

/// Timeline rows are far wider than a `HistPoint`; sized so a full reply stays inside `MAX_FRAME`.
const MAX_TIMELINE_LIMIT: usize = 2_000;

fn serve_query(q: &Query, conn: Option<&Connection>, config_path: &Path, max_age: u64) -> Response {
    match q {
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
        // Same `Snapshot` as the exporter, so `status`, /metrics and push never disagree.
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

fn apply_mutation(m: &Mutation, admin: Admin, config_path: &Path) -> Response {
    let result = match m {
        Mutation::SetMetricsPull { enabled } => {
            persist::set_metrics_pull(config_path, *enabled, None)
        }
        Mutation::AddOrgToken { org, token } => persist::set_org_token(config_path, org, token),
        Mutation::RemoveOrgToken { org } => persist::remove_org_token(config_path, org),
    };
    match result {
        Ok(()) => {
            tracing::info!(
                peer_uid = admin.uid(),
                action = m.action(),
                "ipc: config mutated"
            );
            Response::Mutated
        }
        Err(e) => Response::Error(e.to_string()),
    }
}

/// Read from disk, so a just-persisted token org shows without a restart. Unreadable config
/// ⇒ empty.
fn configured_token_orgs(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path)
        .map(|text| crate::shared::config::token_orgs(&text))
        .unwrap_or_default()
}

fn with_db(conn: Option<&Connection>, f: impl FnOnce(&Connection) -> Response) -> Response {
    match conn {
        Some(c) => f(c),
        None => Response::Error("db unavailable".to_string()),
    }
}

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

    #[test]
    fn a_timeline_limit_is_clamped_server_side() {
        let mut conn = Connection::open_in_memory().unwrap();
        store::schema_for_test(&mut conn);
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
                assert!(!t.transitions.limited);
                assert_eq!(t.samples.map(|s| s.items.len()), Some(2));
            }
            other => panic!("expected a timeline, got {other:?}"),
        }
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

    const ROOT: &Peer = &Peer::Known {
        uid: 0,
        admin: true,
    };
    const MEMBER: &Peer = &Peer::Known {
        uid: 1000,
        admin: true,
    };
    const NOBODY: &Peer = &Peer::Known {
        uid: 1000,
        admin: false,
    };
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
        assert!(matches!(
            handle(&Request::Query(Query::ConfiguredTokenOrgs), None, NOBODY, &noconf(), MAX_AGE),
            Response::ConfiguredTokenOrgs(orgs) if orgs.is_empty()
        ));
    }

    #[test]
    fn mutation_denied_for_unauthorized_peer_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let req = Request::Mutate(Mutation::SetMetricsPull { enabled: true });
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
        let req = Request::Mutate(Mutation::SetMetricsPull { enabled: true });
        assert!(matches!(
            handle(&req, None, MEMBER, &cfg, MAX_AGE),
            Response::Mutated
        ));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("enabled = true"), "{text}");
    }
}
