//! IPC integration tests against a private collector. Never use `Client::connect_any`: it
//! probes the system socket first, so a test could drive a production collector and mutate
//! its `/etc` config. Frames are hand-encoded so a bug shared by
//! `write_frame`/`read_frame`, or a serde wire change, still fails here.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Hard-coded, not imported: a wire bump must break this file loudly.
const WIRE: u16 = 11;

struct Collector {
    child: Child,
    dir: PathBuf,
    sock: PathBuf,
    config: PathBuf,
}

impl Collector {
    fn start(name: &str) -> Self {
        // As root the collector's socket is the system one, not the private dir's.
        assert_ne!(
            uzers::get_effective_uid(),
            0,
            "run this suite as a non-root user"
        );
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ghr-stats-it-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let runners = dir.join("runners");
        std::fs::create_dir_all(&runners).expect("create test dir");

        let config = dir.join("config.toml");
        let db = dir.join("t.db");
        std::fs::write(
            &config,
            format!(
                "db_path = {db:?}\n\
                 runner_roots = [{runners:?}]\n\
                 \n[intervals]\n\
                 local_secs = 1\n\
                 api_secs = 3600\n"
            ),
        )
        .expect("write test config");

        let child = Command::new(env!("CARGO_BIN_EXE_ghr-stats"))
            .arg("--config")
            .arg(&config)
            .arg("serve")
            // Keeps this off the system socket.
            .env("XDG_RUNTIME_DIR", &dir)
            // `serve` refuses a terminal stdin.
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

/// Member of the `ghr-stats` group. CI, without the group, exercises refusal.
fn privileged() -> bool {
    Command::new("id")
        .arg("-nG")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .is_some_and(|groups| groups.split_whitespace().any(|g| g == "ghr-stats"))
}

#[test]
fn the_handshake_reports_both_the_wire_and_the_build_version() {
    let c = Collector::start("hello");
    let mut s = c.connect();
    let reply = round_trip(&mut s, &json!({"Hello": {"client": WIRE}}));

    assert_eq!(reply["Hello"]["server"], WIRE);
    let version = reply["Hello"]["version"].as_str().unwrap_or_default();
    assert!(!version.is_empty(), "no collector build version: {reply}");
}

#[test]
fn the_handshake_reports_the_servers_version_rather_than_negotiating() {
    let c = Collector::start("mismatch");
    let mut s = c.connect();
    let reply = round_trip(&mut s, &json!({"Hello": {"client": WIRE + 1}}));
    assert_eq!(reply["Hello"]["server"], WIRE, "{reply}");
}

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

#[test]
fn retention_reports_an_empty_record_as_null_rather_than_epoch_zero() {
    let c = Collector::start("retention");
    let mut s = c.session();

    let r = round_trip(&mut s, &json!({"Query": "Retention"}));
    assert!(
        r.get("Retention").is_some(),
        "expected a Retention reply, got: {r}"
    );
    assert!(
        r["Retention"]["earliest_ts"].is_null(),
        "a store with no samples must answer null, not a timestamp: {r}"
    );
}

#[test]
fn the_mutation_gate_matches_the_callers_privilege() {
    let c = Collector::start("mutate");
    let mut s = c.session();

    let reply = round_trip(
        &mut s,
        &json!({"Mutate": {"SetMetricsPull": {"enabled": true}}}),
    );

    if privileged() {
        assert_eq!(reply, json!("Mutated"), "authorized mutation refused");
        let written = std::fs::read_to_string(&c.config).expect("read config");
        assert!(
            written.contains("[metrics.pull]"),
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
            !written.contains("[metrics.pull]"),
            "refused mutation still touched the config:\n{written}"
        );
    }
}

#[test]
fn a_token_written_over_the_wire_is_never_returned() {
    if !privileged() {
        // Cannot plant a token without the gate; refusal is covered above.
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

/// Asserts promptness: a client served only after `CONN_TIMEOUT` would pass an
/// eventual-success check.
#[test]
fn an_idle_connection_does_not_starve_another_client() {
    let c = Collector::start("fairness");

    let _idle = c.session();

    let started = Instant::now();
    let mut second = c.session();
    let reply = round_trip(&mut second, &json!({"Query": "ConfiguredTokenOrgs"}));
    let elapsed = started.elapsed();

    assert!(
        reply["ConfiguredTokenOrgs"].is_array(),
        "second client was not served: {reply}"
    );
    // Well under CONN_TIMEOUT (5 s), well over scheduling jitter.
    assert!(
        elapsed < Duration::from_secs(2),
        "second client waited {elapsed:?} behind an idle connection — the accept \
         loop is serving connections inline again"
    );
}

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
