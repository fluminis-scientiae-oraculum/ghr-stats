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
use crate::shared::models::ApiState;
use crate::shared::paths::Scope;

/// The server is local and answers immediately; this only bounds a wedged or
/// half-open peer so the TUI's render loop never blocks on it.
const IO_TIMEOUT: Duration = Duration::from_millis(750);

/// A live connection to a collector's IPC socket.
pub(crate) struct Client {
    stream: UnixStream,
    scope: Scope,
}

impl Client {
    /// Try System then User sockets; return the first that connects AND completes
    /// the version handshake. `None` ⇒ Ephemeral (no reachable collector).
    pub(crate) fn connect_any() -> Option<Client> {
        for scope in [Scope::System, Scope::User] {
            match Client::connect(scope) {
                Ok(c) => return Some(c),
                Err(ConnectErr::Unreachable) => {}
                Err(ConnectErr::Denied) => tracing::warn!(
                    ?scope,
                    "collector socket present but connect was denied (EACCES) — \
                     check the unit's RuntimeDirectoryMode / socket permissions"
                ),
                Err(ConnectErr::Version { server }) => tracing::warn!(
                    ?scope,
                    server,
                    client = VERSION,
                    "collector IPC version mismatch — restart the service after upgrading the binary"
                ),
                Err(ConnectErr::Io(e)) => {
                    tracing::debug!(?scope, error = %e, "collector IPC connect failed")
                }
            }
        }
        None
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
        let mut client = Client { stream, scope };
        // Handshake: prove the peer speaks our exact protocol version.
        match client.request(&Request::Hello { client: VERSION })? {
            Response::Hello { server } if server == VERSION => Ok(client),
            Response::Hello { server } | Response::VersionMismatch { server } => {
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
}

/// Rebuild the `(org, agent_id) → ApiState` map from the wire's `Vec<ApiRow>`.
/// The org is part of the key because `agent_id` is unique only within an org.
pub(crate) fn api_map(rows: Vec<ApiRow>) -> HashMap<(String, i64), ApiState> {
    rows.into_iter()
        .map(|r| ((r.org, r.agent_id), r.state))
        .collect()
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
