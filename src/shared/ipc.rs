//! The collector↔TUI IPC: a small, synchronous, length-prefixed JSON protocol
//! over a Unix domain socket. No HTTP framework, no async runtime — one frame is
//! a `u32`-LE length followed by a `serde_json` body. The collector (Persistent
//! mode) serves it; the TUI is the only client, and a successful `connect` is
//! itself the Persistent-mode signal.
//!
//! By construction the protocol carries ONLY derived fleet stats: there is no
//! `Request`/`Response` variant that returns a GitHub token or a config value.
//! Every response payload reuses a `shared::models` type verbatim (the shapes the
//! store's read queries return), so the wire types and the query types can never
//! drift apart.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::shared::models::{ApiState, BusyPoint, HistPoint, HostPoint, JobRow};

/// Wire protocol version. Bump on any breaking change to `Request`/`Response`.
/// The same binary ships both halves, so a mismatch means the installed service
/// is older/newer than the TUI binary — restart the service after upgrading.
pub const VERSION: u16 = 1;

/// Reject any frame whose length prefix exceeds this (corrupt/hostile guard),
/// before allocating. 1 MiB is far above any real history response.
const MAX_FRAME: u32 = 1 << 20;

/// A TUI → collector query. One variant per read query the TUI needs, plus a
/// `Hello` handshake that proves the peer speaks our protocol version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello { client: u16 },
    HostSeries { limit: usize },
    BusySeries { limit: usize },
    RunnerHistory { agent_id: i64, limit: usize },
    RecentJobs { limit: usize },
    ActiveJob { runner_name: String },
    LatestApiRunners,
}

/// One runner's GitHub state, paired with its id. A `Vec` of these — not a
/// `HashMap<i64, _>` — crosses the wire, because JSON object keys must be
/// strings; the client rebuilds the map (`ipc::client::api_map`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRow {
    pub agent_id: i64,
    pub state: ApiState,
}

/// A collector → TUI reply. `Error` carries a human string for logging; the TUI
/// falls back to its in-memory rings on any non-data reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { server: u16 },
    VersionMismatch { server: u16 },
    HostSeries(Vec<HostPoint>),
    BusySeries(Vec<BusyPoint>),
    RunnerHistory(Vec<HistPoint>),
    RecentJobs(Vec<JobRow>),
    ActiveJob(Option<JobRow>),
    LatestApiRunners(Vec<ApiRow>),
    Error(String),
}

/// Write one length-prefixed JSON frame: `u32`-LE length, then the body.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|n| *n <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame. Enforces `MAX_FRAME` before allocating.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let msg = Response::BusySeries(vec![BusyPoint {
            ts: 42,
            busy: 3,
            online: 7,
        }]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        // First four bytes are the LE length of the JSON body.
        let declared = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, buf.len() - 4);
        let back: Response = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(back, Response::BusySeries(v) if v.len() == 1 && v[0].online == 7));
    }

    #[test]
    fn oversize_length_prefix_is_rejected_before_alloc() {
        // A hostile 4 GiB length prefix must error, not attempt a huge alloc.
        let mut framed = (u32::MAX).to_le_bytes().to_vec();
        framed.extend_from_slice(b"ignored");
        let err = read_frame::<_, Request>(&mut &framed[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_body_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::HostSeries { limit: 10 }).unwrap();
        buf.truncate(buf.len() - 2); // lose the tail of the body
        assert!(read_frame::<_, Request>(&mut &buf[..]).is_err());
    }
}
