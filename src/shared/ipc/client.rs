//! Client half of the IPC: finds a collector (System scope, then User) and does
//! request/response round-trips over one kept-open `UnixStream`.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::shared::ipc::{self, ApiRow, Mutation, Query, Request, Response, VERSION};
use crate::shared::models::GhView;
use crate::shared::paths::Scope;

/// Budget for the handshake, mutations and instant queries; the TUI render loop waits on these.
const IO_TIMEOUT: Duration = Duration::from_millis(750);

/// Budget for span queries: cost grows with the window and database size, since
/// `--limit` bounds output, not work. Never issued from the TUI render loop.
const SCAN_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Client {
    stream: UnixStream,
    scope: Scope,
    version: String,
}

impl Client {
    /// Probe System then User scope for a collector that completes the handshake.
    pub(crate) fn connect_any() -> Result<Client, EphemeralReason> {
        let mut reason = EphemeralReason::NoCollector;
        for scope in [Scope::System, Scope::User] {
            match Client::connect(scope) {
                Ok(c) => return Ok(c),
                Err(ConnectErr::Unreachable) => {}
                Err(ConnectErr::Denied) => {
                    tracing::warn!(
                        ?scope,
                        "collector socket present but connect was denied (EACCES) — \
                         check the unit's RuntimeDirectoryMode / socket permissions"
                    );
                    reason = EphemeralReason::Denied;
                }
                Err(ConnectErr::Version { server }) => {
                    tracing::warn!(
                        ?scope,
                        server,
                        client = VERSION,
                        "collector IPC version mismatch — re-run `systemd install` from the newer binary"
                    );
                    reason = EphemeralReason::VersionDrift { server };
                }
                Err(ConnectErr::Io(e)) => {
                    tracing::warn!(?scope, error = %e, "collector IPC handshake failed");
                    // Something is listening; keep a more specific reason from the other scope.
                    if reason == EphemeralReason::NoCollector {
                        reason = EphemeralReason::Unusable {
                            detail: e.to_string(),
                        };
                    }
                }
            }
        }
        Err(reason)
    }

    fn connect(scope: Scope) -> Result<Client, ConnectErr> {
        let stream = match UnixStream::connect(scope.socket_path()) {
            Ok(s) => s,
            Err(e) => {
                return Err(match e.kind() {
                    // No file, or a stale socket with no listener.
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                        ConnectErr::Unreachable
                    }
                    io::ErrorKind::PermissionDenied => ConnectErr::Denied,
                    _ => ConnectErr::Io(e),
                });
            }
        };
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let mut client = Client {
            stream,
            scope,
            version: String::new(),
        };
        match client.request(&Request::Hello { client: VERSION })? {
            Response::Hello { server, version } if server == VERSION => {
                client.version = version;
                Ok(client)
            }
            Response::Hello { server, .. } => Err(ConnectErr::Version { server }),
            _ => Err(ConnectErr::Io(io::Error::other(
                "unexpected handshake reply",
            ))),
        }
    }

    pub(crate) fn request(&mut self, req: &Request) -> io::Result<Response> {
        let _ = self.stream.set_read_timeout(Some(read_timeout(req)));
        ipc::write_frame(&mut self.stream, req)?;
        ipc::read_frame(&mut self.stream)
    }

    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    /// `None` when the collector is too old to report its build version.
    pub(crate) fn collector_version(&self) -> Option<&str> {
        (!self.version.is_empty()).then_some(self.version.as_str())
    }
}

/// No `_` arm: a new request must choose the lookup or the scan budget.
fn read_timeout(req: &Request) -> Duration {
    match req {
        Request::Hello { .. } => IO_TIMEOUT,
        Request::Mutate(
            Mutation::SetMetricsPull { .. }
            | Mutation::AddOrgToken { .. }
            | Mutation::RemoveOrgToken { .. },
        ) => IO_TIMEOUT,
        Request::Query(Query::Timeline(_)) => SCAN_TIMEOUT,
        Request::Query(
            Query::HostSeries { .. }
            | Query::BusySeries { .. }
            | Query::RunnerHistory { .. }
            | Query::RecentJobs { .. }
            | Query::LatestJob { .. }
            | Query::LatestApiRunners
            | Query::FleetStatus
            // Covering-index `min(ts)`: answers about the whole record without scanning it.
            | Query::Retention
            | Query::RunnerStates
            | Query::ConfiguredTokenOrgs,
        ) => IO_TIMEOUT,
    }
}

pub(crate) fn api_map(rows: Vec<ApiRow>) -> HashMap<(String, i64), GhView> {
    rows.into_iter()
        .map(|r| ((r.org, r.agent_id), r.view))
        .collect()
}

/// Why the dashboard is running Ephemeral.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EphemeralReason {
    NoCollector,
    /// Collector speaks a different IPC [`VERSION`]; usually an upgraded binary whose
    /// service was not restarted.
    VersionDrift {
        server: u16,
    },
    /// Socket exists but connect was refused with `EACCES`.
    Denied,
    /// Connected, but the handshake failed; `detail` is the underlying error verbatim.
    Unusable {
        detail: String,
    },
    /// Handshake succeeded but the query failed (usually a collector database error).
    QueryFailed,
}

impl EphemeralReason {
    /// Stable machine-readable cause token.
    pub(crate) fn word(&self) -> &'static str {
        match self {
            EphemeralReason::NoCollector => "no-collector",
            EphemeralReason::VersionDrift { .. } => "version-drift",
            EphemeralReason::Denied => "denied",
            EphemeralReason::Unusable { .. } => "handshake-failed",
            EphemeralReason::QueryFailed => "query-failed",
        }
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            EphemeralReason::Unusable { detail } => Some(detail.as_str()),
            _ => None,
        }
    }
}

/// Which side of a collector/binary mismatch is the older build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Behind {
    Service,
    Binary,
}

impl Behind {
    pub(crate) fn of_wire(server: u16) -> Behind {
        if server < VERSION {
            Behind::Service
        } else {
            Behind::Binary
        }
    }

    /// `None` when the builds match or either is not a plain `x.y.z`.
    pub(crate) fn of_builds(service: &str, binary: &str) -> Option<Behind> {
        let parse =
            |v: &str| -> Option<Vec<u64>> { v.split('.').map(|n| n.parse().ok()).collect() };
        let (service, binary) = (parse(service)?, parse(binary)?);
        (service != binary).then(|| {
            if service < binary {
                Behind::Service
            } else {
                Behind::Binary
            }
        })
    }

    /// The service runs its own installed copy, so a restart alone never picks up an upgrade.
    pub(crate) fn remedy(self) -> String {
        match self {
            Behind::Service => format!(
                "re-install the service from this binary: {} (or `systemd install --user` for a \
                 user service)",
                crate::shared::privileged::sudo_hint("systemd install --system")
            ),
            Behind::Binary => {
                "this binary is older than the running service: upgrade it, or use the \
                 service's installed copy"
                    .to_string()
            }
        }
    }
}

/// The remedy when the builds differ but which is older is unknown.
pub(crate) const REINSTALL_FROM_NEWER: &str =
    "re-run `systemd install` from the newer of the two binaries";

enum ConnectErr {
    Unreachable,
    Denied,
    Version { server: u16 },
    Io(io::Error),
}

impl From<io::Error> for ConnectErr {
    fn from(e: io::Error) -> Self {
        ConnectErr::Io(e)
    }
}
