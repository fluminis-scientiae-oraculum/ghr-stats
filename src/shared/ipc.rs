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

use crate::shared::models::{ApiState, BusyPoint, HistPoint, HostPoint, JobRow, RunnerState};

/// Wire protocol version. Bump on any breaking change to `Request`/`Response`.
/// The same binary ships both halves, so a mismatch means the installed service
/// is older/newer than the TUI binary — restart the service after upgrading.
/// v2: added `RunnerStates` (persisted liveness edges for the "For" duration).
/// v3: added authorized config mutations (`SetMetricsPull`, `AddOrgToken`).
/// v4: added `ConfiguredTokenOrgs` (presence-only view of the root config's
///     configured org logins — so a non-root TUI reflects the true PAT state).
/// v5: split `Request` into `Query`/`Mutate` so authz is structural (mutations
///     are unreachable except past the gate).
/// v6: `ActiveJob` → `LatestJob` — the runner-detail job line now shows the most
///     recent job (running OR last completed), not only an in-flight one.
/// v7: added `RemoveOrgToken` (drop an org's PAT + forget the org) — the config
///     wizard's `[r]` action.
pub const VERSION: u16 = 7;

/// Reject any frame whose length prefix exceeds this (corrupt/hostile guard),
/// before allocating. 1 MiB is far above any real history response.
const MAX_FRAME: u32 = 1 << 20;

/// A TUI → collector request. The three-way split is deliberate and *structural*
/// (not cosmetic): the collector routes purely on this shape — `Query` is served
/// with no authorization, `Mutate` is reachable ONLY past the peer-cred authz
/// gate. So an unauthorized mutation, or a mutation that forgot the gate, is
/// unrepresentable — a compile-time property, not something a test must guard.
/// Adding a variant to either inner enum forces the matching handler arm
/// (exhaustive `match`, no `unreachable!`), so "changed here, forgot there"
/// becomes a build error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Version handshake — proves the peer speaks our protocol version.
    Hello { client: u16 },
    /// A read. Never authorized (carries only derived fleet stats + config
    /// presence — never a secret).
    Query(Query),
    /// A config write. Authorized (uid 0 or the `ghr-stats` group) or refused.
    Mutate(Mutation),
}

/// The read queries the TUI issues. Unauthenticated by construction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Query {
    HostSeries { limit: usize },
    BusySeries { limit: usize },
    RunnerHistory { agent_id: i64, limit: usize },
    RecentJobs { limit: usize },
    LatestJob { runner_name: String },
    LatestApiRunners,
    /// Persisted per-runner liveness edges (survive restarts) — for the "For"
    /// duration. Falls back to the TUI's in-memory edge when absent.
    RunnerStates,
    /// The org logins that have a configured read-only PAT — presence ONLY, never
    /// the token. Lets a non-root TUI (which can't read the root-owned /etc config)
    /// show the true configured-token state via the root collector.
    ConfiguredTokenOrgs,
}

/// The config writes the TUI can request. Authorized (uid 0 or `ghr-stats` group)
/// by the single gate on the `Request::Mutate` branch — every present and future
/// variant is behind it, with no per-variant opt-in to forget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Mutation {
    /// Toggle the Prometheus pull endpoint (mirrors the TUI's `[m]`).
    SetMetricsPull { enabled: bool, addr: String },
    /// Add/replace a read-only PAT for an org (mirrors the wizard's `[a]`). The
    /// token is one-way: it is written but never returned in any response.
    AddOrgToken { org: String, token: String },
    /// Remove an org's PAT and forget the org (mirrors the wizard's `[r]`).
    RemoveOrgToken { org: String },
}

impl Mutation {
    /// A stable, payload-free audit label for a mutation (never the org/token) —
    /// the single variant→name map, shared by the collector's deny + apply logs.
    pub fn action(&self) -> &'static str {
        match self {
            Mutation::SetMetricsPull { .. } => "set_metrics_pull",
            Mutation::AddOrgToken { .. } => "add_org_token",
            Mutation::RemoveOrgToken { .. } => "remove_org_token",
        }
    }
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
    LatestJob(Option<JobRow>),
    LatestApiRunners(Vec<ApiRow>),
    /// Persisted liveness edges; `RunnerState.agent_id` is self-keying, so a
    /// `Vec` crosses the wire and the client rebuilds the map.
    RunnerStates(Vec<RunnerState>),
    /// Configured org logins (presence only — no token values ever cross here).
    ConfiguredTokenOrgs(Vec<String>),
    /// A mutation was authorized and persisted.
    Mutated,
    /// A mutation was refused — the peer is neither root nor in `ghr-stats`.
    Denied,
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
        write_frame(&mut buf, &Request::Query(Query::HostSeries { limit: 10 })).unwrap();
        buf.truncate(buf.len() - 2); // lose the tail of the body
        assert!(read_frame::<_, Request>(&mut &buf[..]).is_err());
    }

    #[test]
    fn mutation_request_variants_round_trip() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::Mutate(Mutation::SetMetricsPull {
                enabled: true,
                addr: "127.0.0.1:9477".to_string(),
            }),
        )
        .unwrap();
        let back: Request = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(
            back,
            Request::Mutate(Mutation::SetMetricsPull { enabled: true, addr })
                if addr == "127.0.0.1:9477"
        ));

        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Request::Mutate(Mutation::AddOrgToken {
                org: "acme".to_string(),
                token: "github_pat_ABC".to_string(),
            }),
        )
        .unwrap();
        let back: Request = read_frame(&mut &buf[..]).unwrap();
        assert!(matches!(
            back,
            Request::Mutate(Mutation::AddOrgToken { org, token })
                if org == "acme" && token == "github_pat_ABC"
        ));
    }

    /// The mutation-reply variants carry no payload — structurally, no `Response`
    /// can return a token. This pins that: their JSON bodies mention neither a
    /// token nor a value, only the tag.
    #[test]
    fn mutation_replies_are_payload_free() {
        for resp in [Response::Mutated, Response::Denied] {
            let body = serde_json::to_string(&resp).unwrap();
            assert!(
                body == "\"Mutated\"" || body == "\"Denied\"",
                "mutation reply must be a bare tag, got {body}"
            );
        }
    }
}
