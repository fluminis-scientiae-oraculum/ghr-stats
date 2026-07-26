//! The TUI half of the IPC: connect to a collector's socket (System scope, then
//! User — cross-scope, mirroring `config.rs`'s `pick_installed`) and issue
//! synchronous request/response round-trips over one kept-open `UnixStream`.
//!
//! A successful connect + version handshake is the Persistent-mode signal;
//! anything else (no socket, refused, denied, wrong version) means Ephemeral. A
//! mid-session I/O error drops the client, and the App re-probes on the next
//! refresh — so the collector starting or stopping while the TUI is open is
//! handled without special-casing.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::shared::ipc::{self, ApiRow, Request, Response, VERSION};
use crate::shared::models::GhView;
use crate::shared::paths::Scope;

/// The server is local and answers immediately; this only bounds a wedged or
/// half-open peer so the TUI's render loop never blocks on it.
const IO_TIMEOUT: Duration = Duration::from_millis(750);

/// A live connection to a collector's IPC socket.
pub(crate) struct Client {
    stream: UnixStream,
    scope: Scope,
    /// The COLLECTOR's build version, from the handshake. Empty when talking to
    /// a pre-v9 collector that did not report one.
    version: String,
}

impl Client {
    /// Try System then User sockets; return the first that connects AND completes
    /// the version handshake. `None` ⇒ Ephemeral (no reachable collector).
    /// Probe both scopes for a reachable collector.
    ///
    /// Returns WHY on failure rather than a bare `None`. Flattening the reason
    /// away made a version-drifted collector indistinguishable from no collector
    /// at all: after upgrading the binary without restarting the service, the
    /// dashboard silently dropped to Ephemeral with nothing to explain it.
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
                        "collector IPC version mismatch — restart the service after upgrading the binary"
                    );
                    // Most specific reason wins: a live-but-mismatched collector
                    // is a different (and fixable) problem from an absent one.
                    reason = EphemeralReason::VersionDrift { server };
                }
                Err(ConnectErr::Io(e)) => {
                    tracing::warn!(?scope, error = %e, "collector IPC handshake failed");
                    // The socket accepted us, so something IS listening — this
                    // arm setting no reason at all is what let a live collector
                    // report as absent. Only fill the default in: a `Denied` or
                    // `VersionDrift` found in the other scope is more specific
                    // and must not be downgraded.
                    //
                    // The error rides ON the reason, not just into the log: the
                    // callers that most need it (`doctor`, `explain`) have no log
                    // sink by design, so a warn alone reaches nobody.
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
        // Handshake: prove the peer speaks our exact protocol version.
        match client.request(&Request::Hello { client: VERSION })? {
            Response::Hello { server, version } if server == VERSION => {
                client.version = version;
                Ok(client)
            }
            Response::Hello { server, .. } | Response::VersionMismatch { server } => {
                Err(ConnectErr::Version { server })
            }
            _ => Err(ConnectErr::Io(io::Error::other(
                "unexpected handshake reply",
            ))),
        }
    }

    /// One request → one response, reusing the connection.
    pub(crate) fn request(&mut self, req: &Request) -> io::Result<Response> {
        ipc::write_frame(&mut self.stream, req)?;
        ipc::read_frame(&mut self.stream)
    }

    /// The scope whose collector this client is attached to (for the UI to note
    /// "history is in system scope" when it differs from the TUI's own scope).
    pub(crate) fn scope(&self) -> Scope {
        self.scope
    }

    /// The connected collector's build version. `None` for a pre-v9 collector
    /// that did not report one — which is itself a useful signal.
    pub(crate) fn collector_version(&self) -> Option<&str> {
        (!self.version.is_empty()).then_some(self.version.as_str())
    }
}

/// Rebuild the `(org, agent_id) → ApiState` map from the wire's `Vec<ApiRow>`.
/// The org is part of the key because `agent_id` is unique only within an org.
pub(crate) fn api_map(rows: Vec<ApiRow>) -> HashMap<(String, i64), GhView> {
    rows.into_iter()
        .map(|r| ((r.org, r.agent_id), r.view))
        .collect()
}

/// Why the dashboard is running Ephemeral. Carried on the mode itself so the
/// reason cannot be dropped on the floor between detecting it and showing it.
///
/// Deliberately not `Copy`: [`Self::Unusable`] carries the failure it stands for.
/// That detail used to be `tracing::warn!`-ed beside this value and thrown away,
/// which left `doctor` — the one verb whose whole job is explaining a broken
/// install — reporting `handshake-failed` with nothing to act on, because
/// `doctor` deliberately has no log sink. The same principle the type already
/// rested on applies one level down: a reason that needs a detail must carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EphemeralReason {
    /// No collector socket found in either scope.
    NoCollector,
    /// A collector IS running but speaks a different IPC wire version — nearly
    /// always an upgraded binary whose service was not restarted.
    VersionDrift { server: u16 },
    /// The socket exists but this user may not connect to it.
    Denied,
    /// The socket accepted the connection but the handshake never completed —
    /// an I/O error, or a reply we could not make sense of. Something IS
    /// listening, so "install a collector" is the wrong advice.
    ///
    /// `detail` is the underlying failure, verbatim. It is the difference
    /// between "the collector is broken somehow" and a timeout, a truncated
    /// frame, or a payload this build cannot parse — three different next steps.
    Unusable { detail: String },
    /// The collector completed the handshake but did not answer the query — so
    /// it is running AND speaks our wire version, and the fault is its own
    /// (usually a database error). Distinct from [`Self::NoCollector`] because
    /// the remedy is the opposite: read its journal, do not install anything.
    QueryFailed,
}

impl EphemeralReason {
    /// A stable machine-readable token for the cause, so a caller can branch on
    /// it without matching prose that may be reworded.
    ///
    /// Lives on the reason rather than in the one verb that first rendered it:
    /// `explain` puts it in a finding's evidence and `timeline` puts it on
    /// stderr before exiting, and two spellings of "denied" would be two
    /// vocabularies for one fact.
    pub(crate) fn word(&self) -> &'static str {
        match self {
            EphemeralReason::NoCollector => "no-collector",
            EphemeralReason::VersionDrift { .. } => "version-drift",
            EphemeralReason::Denied => "denied",
            EphemeralReason::Unusable { .. } => "handshake-failed",
            EphemeralReason::QueryFailed => "query-failed",
        }
    }

    /// The underlying failure, when there is one to show. `None` for every
    /// reason whose `word` already says everything knowable about it.
    ///
    /// Returned rather than folded into the word so the two stay separable: an
    /// agent branches on the stable token, a human reads the detail.
    pub(crate) fn detail(&self) -> Option<&str> {
        match self {
            EphemeralReason::Unusable { detail } => Some(detail.as_str()),
            _ => None,
        }
    }
}

/// Why a connect attempt did not yield a Persistent client.
enum ConnectErr {
    /// No socket, or a stale socket with no listener ⇒ Ephemeral.
    Unreachable,
    /// Socket exists but connect was denied (perms) ⇒ surfaced, not silent.
    Denied,
    /// Peer speaks a different protocol version.
    Version { server: u16 },
    /// Any other I/O error.
    Io(io::Error),
}

impl From<io::Error> for ConnectErr {
    fn from(e: io::Error) -> Self {
        ConnectErr::Io(e)
    }
}
