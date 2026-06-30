//! Runner hook install — detect-first, NEVER clobber (operator: "detect →
//! choose per runner").
//!
//! A GitHub runner allows exactly ONE script per `ACTIONS_RUNNER_HOOK_JOB_*`
//! var, and an operator may already use them (e.g. for docker cleanup). So we
//! DETECT the current state and, on a conflict, offer to CHAIN (a wrapper that
//! runs the existing hook then appends our event) or to INSTRUCT (print the
//! snippet to add) — we never overwrite a foreign hook. Our scripts (and the
//! wrapper) preserve the original exit code: a non-zero runner hook fails the job.

use std::path::{Path, PathBuf};

use crate::error::Result;

const STARTED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";
const COMPLETED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_COMPLETED";

/// Our hook scripts, embedded so the binary is self-contained for any adopter.
const STARTED_SCRIPT: &str = include_str!("../../packaging/hooks/job-started.sh");
const COMPLETED_SCRIPT: &str = include_str!("../../packaging/hooks/job-completed.sh");

/// What a runner's hook env vars currently point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStatus {
    /// Both vars point inside our hooks dir.
    Ours,
    /// At least one var points at a foreign script — chain or instruct.
    Foreign,
    /// Neither var is set — a clean install is possible.
    Unset,
    /// The `.env` could not be read (perms); caller may use a heuristic.
    Unreadable,
}

impl HookStatus {
    /// ✓ / ✗ / ? glyph for the dashboard.
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            HookStatus::Ours => "✓",
            HookStatus::Foreign | HookStatus::Unset => "✗",
            HookStatus::Unreadable => "?",
        }
    }
}

/// Where ghr-stats installs its hook scripts (outside any runner `_work`, which
/// a checkout would overwrite). `data_dir` already ends in `ghr-stats`.
pub(crate) fn hooks_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("hooks")
}

/// Read + classify a runner's `.env`. `Unreadable` if it can't be read.
pub(crate) fn detect(install_dir: &Path, our_dir: &Path) -> HookStatus {
    match std::fs::read_to_string(install_dir.join(".env")) {
        Ok(text) => classify(&text, our_dir),
        Err(_) => HookStatus::Unreadable,
    }
}

/// Classify hook state from `.env` text + our hooks dir. Pure.
pub(crate) fn classify(env: &str, our_dir: &Path) -> HookStatus {
    let is_ours = |v: &str| Path::new(v).starts_with(our_dir);
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

/// The value of `.env` key `key` (KEY=VALUE; last wins; quotes stripped).
fn env_value(env: &str, key: &str) -> Option<String> {
    let mut val = None;
    for line in env.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(key)
            && let Some(v) = rest.strip_prefix('=')
        {
            val = Some(v.trim().trim_matches(['"', '\'']).to_string());
        }
    }
    val
}

/// The current hook script paths from `.env` (for chaining onto a foreign hook).
pub(crate) fn current_hook_paths(env: &str) -> (Option<String>, Option<String>) {
    (env_value(env, STARTED_VAR), env_value(env, COMPLETED_VAR))
}

/// Write our two hook scripts into `our_dir` (mode 0755). Returns their paths.
pub(crate) fn install_scripts(our_dir: &Path) -> Result<(PathBuf, PathBuf)> {
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

/// Rewrite `.env` content with the two hook vars pointing at `started`/
/// `completed` (replacing any existing values, preserving other lines). Pure.
pub(crate) fn rewrite_env(existing: &str, started: &Path, completed: &Path) -> String {
    let mut out: Vec<String> = existing
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with(STARTED_VAR) && !t.starts_with(COMPLETED_VAR)
        })
        .map(str::to_string)
        .collect();
    out.push(format!("{STARTED_VAR}={}", started.display()));
    out.push(format!("{COMPLETED_VAR}={}", completed.display()));
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// A chain wrapper: run the operator's existing hook (preserving its exit code,
/// which is the runner's pass/fail signal), then best-effort append our event.
pub(crate) fn render_chain_wrapper(original: &Path, ours: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # ghr-stats hook chain wrapper — runs the existing hook, then records\n\
         # the ghr-stats event (best-effort). Preserves the original's exit code.\n\
         \"{orig}\" \"$@\"; rc=$?\n\
         \"{ours}\" \"$@\" >/dev/null 2>&1 || true\n\
         exit \"$rc\"\n",
        orig = original.display(),
        ours = ours.display(),
    )
}

/// The snippet printed for the "instruct" path (operator adds it to their hook).
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
mod tests {
    use super::*;

    fn our() -> PathBuf {
        PathBuf::from("/var/lib/ghr-stats/hooks")
    }

    #[test]
    fn classify_unset_ours_foreign() {
        assert_eq!(classify("", &our()), HookStatus::Unset);
        let ours = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        assert_eq!(classify(ours, &our()), HookStatus::Ours);
        let foreign = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup-started.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup-completed.sh\n";
        assert_eq!(classify(foreign, &our()), HookStatus::Foreign);
        // one ours + one missing ⇒ not fully ours
        let half = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n";
        assert_eq!(classify(half, &our()), HookStatus::Foreign);
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
        );
        assert!(out.contains("TMPDIR=/var/tmp/runner"));
        assert!(out.contains("KEEP=1"));
        assert!(!out.contains("/old/start.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh"));
    }

    #[test]
    fn chain_wrapper_runs_both_and_preserves_rc() {
        let w = render_chain_wrapper(
            Path::new("/usr/local/sbin/cleanup-started.sh"),
            Path::new("/var/lib/ghr-stats/hooks/job-started.sh"),
        );
        assert!(w.contains("/usr/local/sbin/cleanup-started.sh"));
        assert!(w.contains("/var/lib/ghr-stats/hooks/job-started.sh"));
        assert!(w.contains("exit \"$rc\""));
    }
}
