//! Integration tests: a real collector, a private socket, and the wire spoken
//! from the outside.
//!
//! **Isolation is the first requirement, not a detail.** This suite runs on
//! machines that also run a production collector, and `Client::connect_any`
//! probes the SYSTEM socket (`/run/ghr-stats/serve.sock`) before the user one.
//! A test that used the client could therefore drive the real fleet's collector
//! — and issue mutations against the real `/etc` config. So these tests never
//! call the client: each spawns its own collector under a private
//! `XDG_RUNTIME_DIR` with its own `--config` and database, and connects to that
//! explicit path.
//!
//! **The frames are built by hand, deliberately.** The binary has no library
//! target, so the crate's own types are unreachable here — but that constraint
//! is worth having. Encoding the length prefix and the JSON shapes
//! independently means a bug shared by `write_frame` and `read_frame`, or a
//! serde attribute that changes the wire without changing the Rust types, still
//! fails this file. A test that reuses the implementation it is testing cannot
//! catch that class at all.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The wire version this suite speaks.
///
/// Hard-coded rather than imported — it IS the external contract, and a bump
/// should break this file loudly enough that someone confirms the change was
/// intended rather than incidental.
const WIRE: u16 = 10;

/// A collector running on its own socket, with its own config and database.
struct Collector {
    child: Child,
    dir: PathBuf,
    sock: PathBuf,
    config: PathBuf,
}

impl Collector {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "ghr-stats-it-{}-{}-{name}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");

        let config = dir.join("config.toml");
        let db = dir.join("t.db");
        std::fs::write(
            &config,
            format!(
                "db_path = {:?}\n\
                 runner_roots = []\n\
                 \n[intervals]\n\
                 local_secs = 1\n\
                 api_secs = 3600\n",
                db
            ),
        )
        .expect("write test config");

        let child = Command::new(env!("CARGO_BIN_EXE_ghr-stats"))
            .arg("--config")
            .arg(&config)
            .arg("serve")
            // A private runtime dir is what keeps this off the system socket.
            .env("XDG_RUNTIME_DIR", &dir)
            // `serve` refuses to run attached to a terminal; under `cargo test`
            // from an interactive shell the child would inherit one.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn collector");

        let sock = dir.join("ghr-stats").join("serve.sock");
        let me = Collector {
            child,
            dir,
            sock,
            config,
        };
        me.await_socket();
        me
    }

    /// Wait for the collector to bind. Polls rather than sleeping a fixed span:
    /// a fixed sleep is either flaky on a loaded machine or wasted everywhere.
    fn await_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.sock.exists() && UnixStream::connect(&self.sock).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("collector never bound {}", self.sock.display());
    }

    fn connect(&self) -> UnixStream {
        let s = UnixStream::connect(&self.sock).expect("connect to test collector");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    }

    /// Connect and complete the handshake — the state every real client is in.
    fn session(&self) -> UnixStream {
        let mut s = self.connect();
        let hello = round_trip(&mut s, &json!({"Hello": {"client": WIRE}}));
        assert_eq!(hello["Hello"]["server"], WIRE, "handshake refused: {hello}");
        s
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_frame(s: &mut UnixStream, msg: &Value) {
    let body = serde_json::to_vec(msg).unwrap();
    s.write_all(&(body.len() as u32).to_le_bytes()).unwrap();
    s.write_all(&body).unwrap();
    s.flush().unwrap();
}

fn read_frame(s: &mut UnixStream) -> Value {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).expect("read frame length");
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut body).expect("read frame body");
    serde_json::from_slice(&body).expect("frame is JSON")
}

fn round_trip(s: &mut UnixStream, msg: &Value) -> Value {
    write_frame(s, msg);
    read_frame(s)
}

/// Whether this process would pass the collector's mutation gate: uid 0, or a
/// member of the `ghr-stats` group.
///
/// Resolved the same way the server resolves it (the group database), so the
/// assertions below track the machine they run on. This split is deliberate and
/// both halves are real: a developer in the admin group exercises the
/// authorized path including the config reload, and CI — where no such group
/// exists — exercises the refusal.
fn privileged() -> bool {
    if unsafe_uid() == 0 {
        return true;
    }
    Command::new("id")
        .arg("-nG")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|groups| groups.split_whitespace().any(|g| g == "ghr-stats"))
}

/// The caller's uid, via `id -u` — the crate forbids `unsafe`, so no `getuid`.
fn unsafe_uid() -> u32 {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(u32::MAX)
}

#[test]
fn the_handshake_reports_both_the_wire_and_the_build_version() {
    let c = Collector::start("hello");
    let mut s = c.connect();
    let reply = round_trip(&mut s, &json!({"Hello": {"client": WIRE}}));

    assert_eq!(reply["Hello"]["server"], WIRE);
    // The build version is what makes "you upgraded the binary but did not
    // restart the service" visible; an empty string would hide it.
    let version = reply["Hello"]["version"].as_str().unwrap_or_default();
    assert!(!version.is_empty(), "no collector build version: {reply}");
}

/// The handshake REPORTS; it does not negotiate.
///
/// The collector answers with its own version whatever the client claims, and
/// deciding whether that is acceptable is the client's job. Worth pinning
/// because the opposite is the natural assumption — this test was first written
/// expecting a `VersionMismatch` reply, which the collector never sends. The
/// design is deliberate: the server cannot know which request shapes a differing
/// client will actually use, and reporting its version unconditionally is what
/// lets the client produce `version-drift` with the server's number in it
/// rather than a bare failure.
#[test]
fn the_handshake_reports_the_servers_version_rather_than_negotiating() {
    let c = Collector::start("mismatch");
    let mut s = c.connect();
    let reply = round_trip(&mut s, &json!({"Hello": {"client": WIRE + 1}}));

    assert_eq!(
        reply["Hello"]["server"], WIRE,
        "the collector must report its own version to a differing client: {reply}"
    );
    assert!(
        reply.get("VersionMismatch").is_none(),
        "this collector does not send VersionMismatch: {reply}"
    );
}

/// Reads are unauthenticated BY CONSTRUCTION — `Query` is a separate arm of
/// `Request` that never reaches the authz gate. This pins the behaviour from
/// outside the type system that guarantees it.
#[test]
fn queries_are_answered_without_authorization() {
    let c = Collector::start("query");
    let mut s = c.session();

    let orgs = round_trip(&mut s, &json!({"Query": "ConfiguredTokenOrgs"}));
    assert!(
        orgs["ConfiguredTokenOrgs"].is_array(),
        "expected an org list: {orgs}"
    );

    let series = round_trip(&mut s, &json!({"Query": {"BusySeries": {"limit": 5}}}));
    assert!(
        series["BusySeries"].is_array(),
        "expected a busy series: {series}"
    );

    let status = round_trip(&mut s, &json!({"Query": "FleetStatus"}));
    assert_eq!(
        status["FleetStatus"]["schema_version"], 1,
        "expected a versioned fleet status: {status}"
    );
}

/// The mutation gate, and — when this caller passes it — that a persisted
/// mutation actually reaches the config file. The reload is the whole point of
/// routing config edits through the collector rather than writing `/etc`
/// directly.
#[test]
fn the_mutation_gate_matches_the_callers_privilege() {
    let c = Collector::start("mutate");
    let mut s = c.session();

    let reply = round_trip(
        &mut s,
        &json!({"Mutate": {"SetMetricsPull": {"enabled": true, "addr": "127.0.0.1:9999"}}}),
    );

    if privileged() {
        assert_eq!(reply, json!("Mutated"), "authorized mutation refused");
        let written = std::fs::read_to_string(&c.config).expect("read config");
        assert!(
            written.contains("9999"),
            "mutation was acknowledged but not persisted:\n{written}"
        );
    } else {
        assert_eq!(
            reply,
            json!("Denied"),
            "unauthorized mutation was not refused"
        );
        let written = std::fs::read_to_string(&c.config).expect("read config");
        assert!(
            !written.contains("9999"),
            "refused mutation still touched the config:\n{written}"
        );
    }
}

/// A token crosses the wire INBOUND and must never come back out. The org list
/// is presence-only by design, so a non-root TUI can see which orgs have a PAT
/// without ever seeing one.
#[test]
fn a_token_written_over_the_wire_is_never_returned() {
    if !privileged() {
        // Without the gate we cannot plant a token to look for. The refusal
        // path is covered by the test above.
        return;
    }
    let c = Collector::start("token");
    let mut s = c.session();
    const SECRET: &str = "github_pat_integration_test_sentinel";

    let reply = round_trip(
        &mut s,
        &json!({"Mutate": {"AddOrgToken": {"org": "acme", "token": SECRET}}}),
    );
    assert_eq!(reply, json!("Mutated"), "could not plant a token: {reply}");

    let orgs = round_trip(&mut s, &json!({"Query": "ConfiguredTokenOrgs"}));
    assert_eq!(orgs["ConfiguredTokenOrgs"][0], "acme");
    assert!(
        !orgs.to_string().contains(SECRET),
        "the org list leaked a token: {orgs}"
    );

    let status = round_trip(&mut s, &json!({"Query": "FleetStatus"}));
    assert!(
        !status.to_string().contains(SECRET),
        "the fleet status leaked a token"
    );
}

/// The regression for the total IPC lockout fixed in `4b3b490`.
///
/// Asserts PROMPTNESS, not eventual success — that distinction is the test.
/// Before the fix the accept loop ran `serve_conn` inline, and `serve_conn`
/// loops until its client hangs up or its read times out (`CONN_TIMEOUT`, 5 s).
/// So a client that merely HELD a connection open — exactly what the dashboard
/// does between refreshes — starved every other caller. A test that waited for
/// eventual success would have passed against the broken design: the second
/// client does get served, five seconds later. Only a deadline catches it.
#[test]
fn an_idle_connection_does_not_starve_another_client() {
    let c = Collector::start("fairness");

    // A client that handshakes and then sits there, sending nothing further.
    let _idle = c.session();

    let started = Instant::now();
    let mut second = c.session();
    let reply = round_trip(&mut second, &json!({"Query": "ConfiguredTokenOrgs"}));
    let elapsed = started.elapsed();

    assert!(
        reply["ConfiguredTokenOrgs"].is_array(),
        "second client was not served: {reply}"
    );
    // Comfortably under CONN_TIMEOUT (5 s) so the assertion means "was not
    // waiting on the idle connection", and comfortably over any plausible
    // scheduling delay so it is not flaky.
    assert!(
        elapsed < Duration::from_secs(2),
        "second client waited {elapsed:?} behind an idle connection — the accept \
         loop is serving connections inline again"
    );
}

/// Several held-open connections must not wedge the collector either — the
/// dashboard, a `tail`, and a `status` can legitimately overlap.
#[test]
fn concurrent_clients_are_all_served() {
    let c = Collector::start("concurrent");
    let idle: Vec<UnixStream> = (0..4).map(|_| c.session()).collect();

    let started = Instant::now();
    for _ in 0..3 {
        let mut s = c.session();
        let reply = round_trip(&mut s, &json!({"Query": {"BusySeries": {"limit": 1}}}));
        assert!(reply["BusySeries"].is_array(), "not served: {reply}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "serving overlapping clients took {:?}",
        started.elapsed()
    );
    drop(idle);
}
