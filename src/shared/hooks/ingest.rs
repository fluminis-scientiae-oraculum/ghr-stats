//! A runner's job-event log: NDJSON lines its hooks append, tailed from a
//! persisted byte offset.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

use crate::shared::runner_files;

/// Most bytes read from one log per tick.
const TAIL_CHUNK: u64 = 1024 * 1024;

/// One line of the event log. Timing comes from here; the conclusion is filled
/// later by the API reconcile. The line's `runner` field is ignored: the runner
/// is whoever owns the log.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum HookEvent {
    Started(JobRef),
    Completed(JobRef),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JobRef {
    pub ts: i64,
    pub repo: String,
    pub run_id: i64,
    #[serde(default = "one")]
    pub run_attempt: i64,
    #[serde(default)]
    pub job: String,
}

fn one() -> i64 {
    1
}

impl HookEvent {
    pub fn job(&self) -> &JobRef {
        match self {
            HookEvent::Started(j) | HookEvent::Completed(j) => j,
        }
    }
}

/// Parse one line from the log of a runner registered to `owner`. Anything that
/// is not an event for one of `owner`'s repositories yields `None`.
pub fn parse_event_line(line: &str, owner: &str) -> Option<HookEvent> {
    let event: HookEvent = serde_json::from_str(line.trim()).ok()?;
    is_repo_of(&event.job().repo, owner).then_some(event)
}

fn is_repo_of(repo: &str, owner: &str) -> bool {
    let Some((o, name)) = repo.split_once('/') else {
        return false;
    };
    o.eq_ignore_ascii_case(owner)
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Complete lines of the event log in `dir` from `offset`, one chunk at most.
/// Returns the events and the offset past the last consumed line. A shrunken file
/// restarts from 0; a full chunk with no newline is skipped whole.
pub fn tail_events(dir: &Path, owner: &str, offset: u64) -> (Vec<HookEvent>, u64) {
    let Ok((mut file, meta)) = runner_files::open(dir, super::RUNNER_EVENT_LOG) else {
        return (Vec::new(), offset);
    };
    let start = if meta.len() < offset { 0 } else { offset };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (Vec::new(), start);
    }
    let mut buf = Vec::new();
    if file.take(TAIL_CHUNK).read_to_end(&mut buf).is_err() {
        return (Vec::new(), start);
    }
    let consumed = match buf.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        None if buf.len() as u64 == TAIL_CHUNK => buf.len(),
        None => 0,
    };
    let events = buf[..consumed]
        .split(|b| *b == b'\n')
        .filter_map(|l| std::str::from_utf8(l).ok())
        .filter_map(|l| parse_event_line(l, owner))
        .collect();
    (events, start + consumed as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const OWNER: &str = "example-org";

    fn line(phase: &str, run_id: i64, repo: &str) -> String {
        format!(
            r#"{{"phase":"{phase}","ts":{run_id},"repo":"{repo}","run_id":{run_id},"job":"build"}}"#
        )
    }

    fn log_in(dir: &Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(crate::shared::hooks::RUNNER_EVENT_LOG))
            .unwrap()
    }

    #[test]
    fn only_events_for_the_owners_repos_parse() {
        let started = parse_event_line(&line("started", 1, "example-org/foo"), OWNER).unwrap();
        assert!(
            matches!(started, HookEvent::Started(ref j) if j.run_id == 1 && j.run_attempt == 1)
        );
        assert!(parse_event_line(&line("completed", 1, "Example-Org/foo"), OWNER).is_some());

        for bad in [
            line("started", 1, "other-org/foo"),
            line("started", 1, "example-org/../x"),
            line("started", 1, "example-org"),
            line("queued", 1, "example-org/foo"),
            "not json".to_string(),
            String::new(),
        ] {
            assert_eq!(parse_event_line(&bad, OWNER), None, "{bad}");
        }
    }

    #[test]
    fn tail_consumes_whole_lines_and_skips_bad_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = log_in(dir.path());
        writeln!(f, "{}", line("started", 1, "example-org/a")).unwrap();
        f.write_all(b"\xff\xfe not utf-8\n").unwrap();
        write!(f, r#"{{"phase":"started","ts":2,"#).unwrap();

        let (events, off) = tail_events(dir.path(), OWNER, 0);
        assert_eq!(events.len(), 1);

        writeln!(f, r#""repo":"example-org/b","run_id":2}}"#).unwrap();
        let (events, off2) = tail_events(dir.path(), OWNER, off);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].job().run_id, 2);

        let (events, off3) = tail_events(dir.path(), OWNER, off2);
        assert!(events.is_empty());
        assert_eq!(off3, off2);
    }

    #[test]
    fn a_chunk_with_no_newline_is_skipped_and_truncation_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = log_in(dir.path());
        f.write_all(&vec![b'x'; TAIL_CHUNK as usize + 10]).unwrap();
        let (events, off) = tail_events(dir.path(), OWNER, 0);
        assert!(events.is_empty());
        assert_eq!(off, TAIL_CHUNK);

        f.set_len(0).unwrap();
        let (_, off) = tail_events(dir.path(), OWNER, off);
        assert_eq!(off, 0);
    }

    #[test]
    fn a_symlinked_log_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(
            &target,
            format!("{}\n", line("started", 1, "example-org/a")),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            &target,
            dir.path().join(crate::shared::hooks::RUNNER_EVENT_LOG),
        )
        .unwrap();
        assert_eq!(tail_events(dir.path(), OWNER, 0), (Vec::new(), 0));
    }
}
