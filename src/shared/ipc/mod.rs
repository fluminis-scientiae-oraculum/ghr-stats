//! Collector↔client IPC: synchronous, length-prefixed JSON over a Unix socket.
//! A frame is a `u32`-LE length then a `serde_json` body; one request, one response.
//! No variant carries a GitHub token or config value.
//! No subscribe path: the accept loop drops callers past `MAX_CONNS`, so long-lived
//! streams would lock out other clients; live feeds poll a `Query` with a cursor.

pub mod client;

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::shared::models::timeline::{Timeline, TimelineQuery};
use crate::shared::models::{
    BusyPoint, FleetStatus, GhView, HistPoint, HostPoint, JobRow, RunnerState,
};

/// Wire protocol version; client and collector must match (checked by `Hello`).
pub const VERSION: u16 = 10;

const MAX_FRAME: u32 = 1 << 20;

/// `Query` is served unauthenticated; `Mutate` is reachable only past the peer-cred authz gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// `client` is the caller's [`VERSION`].
    Hello {
        client: u16,
    },
    Query(Query),
    /// Allowed for uid 0 or the `ghr-stats` group; otherwise `Response::Denied`.
    Mutate(Mutation),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Query {
    HostSeries {
        limit: usize,
    },
    BusySeries {
        limit: usize,
    },
    RunnerHistory {
        dir: String,
        limit: usize,
    },
    RecentJobs {
        limit: usize,
    },
    LatestJob {
        runner_name: String,
    },
    LatestApiRunners,
    /// Machine-facing fleet snapshot with the collector-computed health verdict.
    FleetStatus,
    /// What changed over a window, optionally with the samples underneath it.
    Timeline(TimelineQuery),
    /// Where the retained record starts.
    Retention,
    /// Persisted per-runner liveness edges; they survive collector restarts.
    RunnerStates,
    /// Org logins with a configured PAT; presence only, never the token.
    ConfiguredTokenOrgs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Mutation {
    /// Toggle the Prometheus pull endpoint.
    SetMetricsPull { enabled: bool, addr: String },
    /// Add or replace an org's PAT; never returned in any response.
    AddOrgToken { org: String, token: String },
    /// Remove an org's PAT and forget the org.
    RemoveOrgToken { org: String },
}

impl Mutation {
    /// Audit-log label; never includes the org or token.
    pub fn action(&self) -> &'static str {
        match self {
            Mutation::SetMetricsPull { .. } => "set_metrics_pull",
            Mutation::AddOrgToken { .. } => "add_org_token",
            Mutation::RemoveOrgToken { .. } => "remove_org_token",
        }
    }
}

/// One runner's GitHub view keyed by `(org, agent_id)`; `agent_id` is unique only per org.
/// Sent as a `Vec` because JSON object keys must be strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRow {
    pub agent_id: i64,
    pub org: String,
    /// Freshness is decided by the collector; clients render it, never re-derive it.
    pub view: GhView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello {
        server: u16,
        /// Collector build version, not [`VERSION`]. `serde(default)` so older
        /// collectors that omit it still produce a clean version mismatch.
        #[serde(default)]
        version: String,
    },
    HostSeries(Vec<HostPoint>),
    BusySeries(Vec<BusyPoint>),
    RunnerHistory(Vec<HistPoint>),
    RecentJobs(Vec<JobRow>),
    LatestJob(Option<JobRow>),
    LatestApiRunners(Vec<ApiRow>),
    FleetStatus(Box<FleetStatus>),
    Timeline(Box<Timeline>),
    /// Oldest retained sample; `None` when nothing has been sampled yet.
    Retention {
        earliest_ts: Option<i64>,
    },
    RunnerStates(Vec<RunnerState>),
    ConfiguredTokenOrgs(Vec<String>),
    Mutated,
    Denied,
    Error(String),
}

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
    use crate::shared::models::GhCount;

    #[test]
    fn frame_round_trips() {
        let msg = Response::BusySeries(vec![BusyPoint {
            ts: 42,
            busy: 3,
            online: 7,
            github: GhCount::new(5, 7),
        }]);
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let declared = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(declared, buf.len() - 4);
        let back: Response = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(back, Response::BusySeries(v) if v.len() == 1 && v[0].online == 7));
    }

    #[test]
    fn oversize_length_prefix_is_rejected_before_alloc() {
        let mut framed = (u32::MAX).to_le_bytes().to_vec();
        framed.extend_from_slice(b"ignored");
        let err = read_frame::<_, Request>(&mut &framed[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_body_errors() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Query(Query::HostSeries { limit: 10 })).unwrap();
        buf.truncate(buf.len() - 2);
        assert!(read_frame::<_, Request>(&mut &buf[..]).is_err());
    }
}
