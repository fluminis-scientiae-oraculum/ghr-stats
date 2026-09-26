//! Runner hook install: detect first, never clobber a foreign hook (chain or instruct instead).
//! A runner allows one script per `ACTIONS_RUNNER_HOOK_JOB_*` var; a non-zero hook fails the job.

use std::path::{Path, PathBuf};

use crate::shared::error::Result;

pub(crate) mod chain;

pub(crate) use chain::{original_from_wrapper, plan_chain_slot};

const STARTED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";
const COMPLETED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_COMPLETED";
/// Read by our hook scripts; points at [`crate::shared::hooks::runner_event_log`].
const EVENT_LOG_VAR: &str = "GHR_STATS_EVENT_LOG";

const STARTED_SCRIPT: &str = include_str!("../../../../packaging/hooks/job-started.sh");
const COMPLETED_SCRIPT: &str = include_str!("../../../../packaging/hooks/job-completed.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStatus {
    /// Both vars point inside one of our hooks dirs.
    Ours,
    /// At least one var points at a foreign script.
    Foreign,
    Unset,
    Unreadable,
}

/// Outside any runner `_work`, which a checkout overwrites.
pub(crate) fn hooks_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("hooks")
}

/// Accepts any scope's hooks dir: the TUI usually runs non-root while hooks live in the
/// system scope.
pub(crate) fn detect(install_dir: &Path, our_dirs: &[PathBuf]) -> HookStatus {
    match super::env::read(install_dir) {
        Ok(env) => classify(&env.text, our_dirs),
        Err(_) => HookStatus::Unreadable,
    }
}

pub(crate) fn classify(env: &str, our_dirs: &[PathBuf]) -> HookStatus {
    let is_ours = |v: &str| is_directly_in(Path::new(v), our_dirs);
    match (env_value(env, STARTED_VAR), env_value(env, COMPLETED_VAR)) {
        (None, None) => HookStatus::Unset,
        (s, c) => {
            let ours = s.as_deref().is_some_and(is_ours) && c.as_deref().is_some_and(is_ours);
            if ours {
                HookStatus::Ours
            } else {
                HookStatus::Foreign
            }
        }
    }
}

pub(crate) fn is_directly_in(path: &Path, dirs: &[PathBuf]) -> bool {
    path.parent().is_some_and(|p| dirs.iter().any(|d| p == d))
}

/// Named after the install dir: runner names repeat across orgs, dirs do not.
pub(crate) fn chain_wrapper_paths(our_dir: &Path, runner_dir: &Path) -> [PathBuf; 2] {
    let slug: String = runner_dir
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    [
        our_dir.join(format!("chain-{slug}-started.sh")),
        our_dir.join(format!("chain-{slug}-completed.sh")),
    ]
}

/// Every other `.env` line is the operator's and must survive every rewrite.
const OUR_VARS: [&str; 3] = [STARTED_VAR, COMPLETED_VAR, EVENT_LOG_VAR];

/// The value if `line` assigns exactly `key`; a longer var sharing the prefix is a different var.
fn assignment<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    line.strip_prefix(key)?.strip_prefix('=')
}

fn assigns_any(line: &str, keys: &[&str]) -> bool {
    keys.iter().any(|key| assignment(line, key).is_some())
}

/// Last assignment wins; quotes stripped.
fn env_value(env: &str, key: &str) -> Option<String> {
    env.lines()
        .filter_map(|l| assignment(l, key))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
        .next_back()
}

pub(crate) fn current_hook_paths(env: &str) -> (Option<String>, Option<String>) {
    (env_value(env, STARTED_VAR), env_value(env, COMPLETED_VAR))
}

pub(crate) fn install_scripts(
    our_dir: &Path,
    _root: &crate::shared::privileged::Root,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(our_dir)?;
    let started = our_dir.join("job-started.sh");
    let completed = our_dir.join("job-completed.sh");
    write_script_file(&started, STARTED_SCRIPT)?;
    write_script_file(&completed, COMPLETED_SCRIPT)?;
    Ok((started, completed))
}

fn write_script_file(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o755)
        .open(path)?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

/// Points both hook vars at `started`/`completed`, keeping other lines. Always drops a prior
/// [`EVENT_LOG_VAR`]; `Some(log)` re-adds it (install), `None` leaves it out (restore).
pub(crate) fn rewrite_env(
    existing: &str,
    started: &Path,
    completed: &Path,
    event_log: Option<&Path>,
) -> String {
    let mut out: Vec<String> = existing
        .lines()
        .filter(|l| !assigns_any(l, &OUR_VARS))
        .map(str::to_string)
        .collect();
    out.push(format!("{STARTED_VAR}={}", started.display()));
    out.push(format!("{COMPLETED_VAR}={}", completed.display()));
    if let Some(log) = event_log {
        out.push(format!("{EVENT_LOG_VAR}={}", log.display()));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Inverse of [`rewrite_env`] for a fresh install; a chained runner is restored via
/// [`rewrite_env`] with the recovered originals instead.
pub(crate) fn remove_hook_vars(existing: &str) -> String {
    let kept: Vec<&str> = existing
        .lines()
        .filter(|l| !assigns_any(l, &OUR_VARS))
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let mut s = kept.join("\n");
    s.push('\n');
    s
}

/// Sets only [`EVENT_LOG_VAR`], leaving the hook vars alone; `None` when already correct.
pub(crate) fn ensure_event_log(existing: &str, log: &Path) -> Option<String> {
    let want = log.display().to_string();
    if env_value(existing, EVENT_LOG_VAR).as_deref() == Some(want.as_str()) {
        return None;
    }
    let mut out: Vec<String> = existing
        .lines()
        .filter(|l| !assigns_any(l, &[EVENT_LOG_VAR]))
        .map(str::to_string)
        .collect();
    out.push(format!("{EVENT_LOG_VAR}={want}"));
    let mut s = out.join("\n");
    s.push('\n');
    Some(s)
}

pub(crate) fn instruct_snippet(our_dir: &Path) -> String {
    let started = our_dir.join("job-started.sh");
    let completed = our_dir.join("job-completed.sh");
    format!(
        "Keep your existing hooks and add ghr-stats event logging by appending one\n\
         line to each (it always exits 0, so it cannot fail a job):\n\
         \n  # in your JOB_STARTED hook:\n  \"{s}\" \"$@\" || true\n\
         \n  # in your JOB_COMPLETED hook:\n  \"{c}\" \"$@\" || true\n",
        s = started.display(),
        c = completed.display(),
    )
}

#[cfg(test)]
mod fixtures {
    use std::path::PathBuf;

    pub(super) fn our() -> PathBuf {
        PathBuf::from("/var/lib/ghr-stats/hooks")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::our;
    use super::*;

    #[test]
    fn classify_unset_ours_foreign() {
        assert_eq!(classify("", &[our()]), HookStatus::Unset);
        let ours = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        assert_eq!(classify(ours, &[our()]), HookStatus::Ours);
        let foreign = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup-started.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup-completed.sh\n";
        assert_eq!(classify(foreign, &[our()]), HookStatus::Foreign);
        let half = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n";
        assert_eq!(classify(half, &[our()]), HookStatus::Foreign);
    }

    #[test]
    fn classify_in_treats_any_scope_dir_as_ours() {
        let sys = PathBuf::from("/var/lib/ghr-stats/hooks");
        let usr = PathBuf::from("/home/u/.local/share/ghr-stats/hooks");
        let dirs = [usr.clone(), sys.clone()];
        let clean = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                     ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        assert_eq!(classify(clean, &dirs), HookStatus::Ours);
        let chained = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/chain-r1-started.sh\n\
             ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/chain-r1-completed.sh\n";
        assert_eq!(classify(chained, &dirs), HookStatus::Ours);
        assert_eq!(
            classify(clean, std::slice::from_ref(&usr)),
            HookStatus::Foreign
        );
        let foreign = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup.sh\n";
        assert_eq!(classify(foreign, &[usr, sys]), HookStatus::Foreign);
    }

    #[test]
    fn env_value_last_wins_and_strips_quotes() {
        let env = "FOO=bar\nKEY=\"a\"\nKEY=b\n# KEY=c\nTMPDIR=/x\n";
        assert_eq!(env_value(env, "KEY").as_deref(), Some("b"));
        assert_eq!(env_value(env, "MISSING"), None);
    }

    #[test]
    fn rewrite_env_replaces_and_preserves() {
        let existing =
            "TMPDIR=/var/tmp/runner\nACTIONS_RUNNER_HOOK_JOB_STARTED=/old/start.sh\nKEEP=1\n";
        let out = rewrite_env(
            existing,
            Path::new("/h/job-started.sh"),
            Path::new("/h/job-completed.sh"),
            Some(Path::new(
                "/srv/actions-runner/runner-01/.ghr-stats-events.ndjson",
            )),
        );
        assert!(out.contains("TMPDIR=/var/tmp/runner"));
        assert!(out.contains("KEEP=1"));
        assert!(!out.contains("/old/start.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh"));
        assert!(out.contains(
            "GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson"
        ));
    }

    #[test]
    fn rewrite_env_none_strips_our_event_log_var() {
        let existing = "KEEP=1\n\
                        GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson\n";
        let out = rewrite_env(
            existing,
            Path::new("/usr/local/sbin/orig-started.sh"),
            Path::new("/usr/local/sbin/orig-completed.sh"),
            None,
        );
        assert!(out.contains("KEEP=1"));
        assert!(!out.contains("GHR_STATS_EVENT_LOG"));
    }

    #[test]
    fn prefix_sharing_operator_vars_survive_every_rewrite_path() {
        let existing = "ACTIONS_RUNNER_HOOK_JOB_STARTED_EXTRA=/op/extra.sh\n\
                        GHR_STATS_EVENT_LOG_ARCHIVE=/op/archive.ndjson\n\
                        ACTIONS_RUNNER_HOOK_JOB_STARTED=/old/start.sh\n\
                        GHR_STATS_EVENT_LOG=/old/events.ndjson\n";

        assert_eq!(
            env_value(existing, EVENT_LOG_VAR).as_deref(),
            Some("/old/events.ndjson")
        );

        for out in [
            rewrite_env(existing, Path::new("/h/s.sh"), Path::new("/h/c.sh"), None),
            remove_hook_vars(existing),
            ensure_event_log(existing, Path::new("/new/events.ndjson")).expect("stale → rewritten"),
        ] {
            assert!(
                out.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED_EXTRA=/op/extra.sh"),
                "operator var dropped by prefix match:\n{out}"
            );
            assert!(
                out.contains("GHR_STATS_EVENT_LOG_ARCHIVE=/op/archive.ndjson"),
                "operator var dropped by prefix match:\n{out}"
            );
            assert!(
                !out.contains("/old/events.ndjson"),
                "stale value kept:\n{out}"
            );
        }
    }

    #[test]
    fn remove_hook_vars_drops_all_three_and_preserves_others() {
        let existing = "TMPDIR=/var/tmp/runner\n\
                        ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh\n\
                        KEEP=1\n\
                        GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson\n\
                        ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh\n";
        let out = remove_hook_vars(existing);
        assert_eq!(out, "TMPDIR=/var/tmp/runner\nKEEP=1\n");
        let only = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh\n\
                    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh\n\
                    GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson\n";
        assert_eq!(remove_hook_vars(only), "");
    }

    #[test]
    fn ensure_event_log_adds_when_missing_fixes_stale_noops_when_correct() {
        let log = Path::new("/srv/actions-runner/runner-01/.ghr-stats-events.ndjson");
        let missing = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/chain-r1-started.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/chain-r1-completed.sh\n";
        let out = ensure_event_log(missing, log).expect("a change was needed");
        assert!(out.contains(
            "GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson"
        ));
        assert!(out.contains("chain-r1-started.sh"));
        let stale = "GHR_STATS_EVENT_LOG=/old/path.ndjson\nKEEP=1\n";
        let fixed = ensure_event_log(stale, log).expect("a change was needed");
        assert!(!fixed.contains("/old/path.ndjson"));
        assert_eq!(fixed.matches("GHR_STATS_EVENT_LOG=").count(), 1);
        assert!(fixed.contains("KEEP=1"));
        assert!(ensure_event_log(&out, log).is_none());
    }

    #[test]
    fn fresh_install_reverses_to_unset() {
        let original = "TMPDIR=/var/tmp/runner\nKEEP=1\n";
        let installed = rewrite_env(
            original,
            Path::new("/var/lib/ghr-stats/hooks/job-started.sh"),
            Path::new("/var/lib/ghr-stats/hooks/job-completed.sh"),
            Some(Path::new(
                "/srv/actions-runner/runner-01/.ghr-stats-events.ndjson",
            )),
        );
        assert_eq!(classify(&installed, &[our()]), HookStatus::Ours);
        assert!(installed.contains("GHR_STATS_EVENT_LOG="));
        assert_eq!(remove_hook_vars(&installed), original);
    }
}
