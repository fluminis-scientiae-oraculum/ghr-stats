//! Ingest of the runner job-event log: an append-only NDJSON file the runner
//! hooks write (one line per job start/completion). We tail it from a persisted
//! byte offset so ingestion is resume-safe across restarts.
//!
//! Hooks run as the *runner* user, the collector as the operator, so the log
//! lives at a shared path (see `config.event_log`). Single-line appends under
//! the pipe-buffer size are atomic, so concurrent writers interleave safely.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

/// One line of the event log. `started`/`completed` carry the same job key;
/// timing comes from here, the conclusion is filled later by the API reconcile.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HookEvent {
    /// "started" | "completed".
    pub phase: String,
    pub ts: i64,
    #[serde(default)]
    pub repo: String,
    pub run_id: i64,
    #[serde(default = "one")]
    pub run_attempt: i64,
    #[serde(default)]
    pub job: String,
    #[serde(default)]
    pub runner: String,
}

fn one() -> i64 {
    1
}

/// Parse a single NDJSON line; blank or malformed lines yield `None`.
pub fn parse_event_line(line: &str) -> Option<HookEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Read complete (newline-terminated) lines from `offset` to EOF. Returns the
/// parsed events and the new offset (advanced only past consumed lines; a
/// partial trailing line is left for next time). If the file shrank (rotated or
/// recreated), we reset to 0 and re-read.
pub fn tail_events(path: &Path, offset: u64) -> (Vec<HookEvent>, u64) {
    let Ok(mut file) = std::fs::File::open(path) else {
        return (Vec::new(), offset); // no log yet — nothing to do
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = if len < offset { 0 } else { offset };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), start);
    }

    let mut buf = String::new();
    if file.read_to_string(&mut buf).is_err() {
        return (Vec::new(), start);
    }

    // Consume only up to the last newline; keep any partial trailing line.
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => 0,
    };
    let events = buf[..consumed]
        .lines()
        .filter_map(parse_event_line)
        .collect();
    (events, start + consumed as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_started_and_completed() {
        let s = parse_event_line(
            r#"{"phase":"started","ts":1700000000,"repo":"example-org/foo","run_id":123,"run_attempt":1,"job":"build","runner":"runner-01"}"#,
        )
        .unwrap();
        assert_eq!(s.phase, "started");
        assert_eq!(s.run_id, 123);
        assert_eq!(s.repo, "example-org/foo");
        // run_attempt defaults to 1 when omitted
        let c = parse_event_line(
            r#"{"phase":"completed","ts":1700000050,"run_id":123,"job":"build","runner":"r"}"#,
        )
        .unwrap();
        assert_eq!(c.run_attempt, 1);
        assert_eq!(c.ts, 1_700_000_050);
        assert_eq!(parse_event_line(""), None);
        assert_eq!(parse_event_line("not json"), None);
    }

    #[test]
    fn tail_consumes_whole_lines_and_advances_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.ndjson");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"phase":"started","ts":1,"run_id":1,"job":"a","runner":"r"}}"#
        )
        .unwrap();
        // a partial trailing line (no newline) must NOT be consumed yet
        write!(f, r#"{{"phase":"started","ts":2,"run_id":2"#).unwrap();
        f.flush().unwrap();

        let (events, off) = tail_events(&path, 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].run_id, 1);

        // complete the second line; tailing from `off` yields only the new one
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, r#","job":"b","runner":"r"}}"#).unwrap();
        let (events2, off2) = tail_events(&path, off);
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].run_id, 2);
        assert!(off2 > off);

        // nothing new
        let (events3, off3) = tail_events(&path, off2);
        assert!(events3.is_empty());
        assert_eq!(off3, off2);
    }

    #[test]
    fn truncation_resets_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.ndjson");
        std::fs::write(&path, "").unwrap();
        // offset points past EOF (file was rotated/recreated smaller)
        let (events, off) = tail_events(&path, 9999);
        assert!(events.is_empty());
        assert_eq!(off, 0);
    }
}
