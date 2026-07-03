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

use crate::shared::error::Result;

const STARTED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";
const COMPLETED_VAR: &str = "ACTIONS_RUNNER_HOOK_JOB_COMPLETED";
/// Env var the hook scripts read for their event-log path. The installer wires it
/// to the runner's own [`crate::shared::hooks::runner_event_log`] so the hook
/// writes a log the runner user owns (the collector reads it as root).
const EVENT_LOG_VAR: &str = "GHR_STATS_EVENT_LOG";

/// Provenance marker written into every chain wrapper so `uninstall` can recover
/// the operator's ORIGINAL hook path unambiguously (a stable comment, not a
/// re-parse of the exec line). Reversing a chained runner must restore this exact
/// path — never leave the runner hookless (the inverse of never-clobber).
const WRAP_MARKER: &str = "# ghr-stats-wraps:";

/// Our hook scripts, embedded so the binary is self-contained for any adopter.
const STARTED_SCRIPT: &str = include_str!("../../../packaging/hooks/job-started.sh");
const COMPLETED_SCRIPT: &str = include_str!("../../../packaging/hooks/job-completed.sh");

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
    detect_in(install_dir, std::slice::from_ref(&our_dir.to_path_buf()))
}

/// Like [`detect`] but classifies "ours" against SEVERAL candidate hooks dirs.
///
/// Detection must be independent of the euid the caller runs under: hooks are
/// always installed by a root process (System scope, `/var/lib/ghr-stats/hooks`),
/// but the read-only TUI is normally run non-root — so a status probe has to
/// consider EVERY scope's hooks dir, not just `Scope::detect()`'s. Passing the
/// current euid's single dir is what made a System-scoped hook read as `Foreign`
/// (or a fresh install read as absent) in a plain `ghr-stats` dashboard.
pub(crate) fn detect_in(install_dir: &Path, our_dirs: &[PathBuf]) -> HookStatus {
    match std::fs::read_to_string(install_dir.join(".env")) {
        Ok(text) => classify_in(&text, our_dirs),
        Err(_) => HookStatus::Unreadable,
    }
}

/// Classify hook state from `.env` text + our hooks dir. Pure.
pub(crate) fn classify(env: &str, our_dir: &Path) -> HookStatus {
    classify_in(env, std::slice::from_ref(&our_dir.to_path_buf()))
}

/// Classify against MULTIPLE candidate hooks dirs — a hook is "ours" if it points
/// under ANY of them (see [`detect_in`] for why every scope must be considered).
/// Pure.
pub(crate) fn classify_in(env: &str, our_dirs: &[PathBuf]) -> HookStatus {
    let is_ours = |v: &str| our_dirs.iter().any(|d| Path::new(v).starts_with(d));
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
/// `completed` (replacing any existing values, preserving other lines). Any
/// prior [`EVENT_LOG_VAR`] is always dropped first; `event_log` re-adds it:
/// - `Some(log)` on **install** — point the runner at its own event log;
/// - `None` on **restore** — strip our var so a reverted foreign `.env` never
///   keeps a `GHR_STATS_EVENT_LOG` line we injected.
///
/// Pure.
pub(crate) fn rewrite_env(
    existing: &str,
    started: &Path,
    completed: &Path,
    event_log: Option<&Path>,
) -> String {
    let mut out: Vec<String> = existing
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with(STARTED_VAR)
                && !t.starts_with(COMPLETED_VAR)
                && !t.starts_with(EVENT_LOG_VAR)
        })
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

/// A chain wrapper: run the operator's existing hook (preserving its exit code,
/// which is the runner's pass/fail signal), then best-effort append our event.
pub(crate) fn render_chain_wrapper(original: &Path, ours: &Path) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # ghr-stats hook chain wrapper — runs the existing hook, then records\n\
         # the ghr-stats event (best-effort). Preserves the original's exit code.\n\
         {WRAP_MARKER} {orig}\n\
         \"{orig}\" \"$@\"; rc=$?\n\
         \"{ours}\" \"$@\" >/dev/null 2>&1 || true\n\
         exit \"$rc\"\n",
        orig = original.display(),
        ours = ours.display(),
    )
}

/// Plan one hook slot (started OR completed) for the CHAIN path. Given the
/// operator's ORIGINAL hook for that slot (if any), our plain script, and where a
/// chain wrapper would live, decide what to wire into `.env` and what wrapper (if
/// any) to write:
/// - original present ⇒ write a wrapper (runs their hook, then ours) and wire the
///   var at the wrapper;
/// - original absent ⇒ nothing to chain for THIS slot, so wire our plain script
///   directly and write no wrapper.
///
/// This is what makes chaining a `Foreign` runner that has only ONE of the two
/// hook vars set safe: the empty slot gets our script directly instead of being
/// pointed at a wrapper that was never written. Pure — the caller does the I/O.
pub(crate) fn plan_chain_slot(
    original: Option<&str>,
    our_script: &Path,
    wrapper_path: &Path,
) -> (PathBuf, Option<(PathBuf, String)>) {
    match original {
        Some(o) => (
            wrapper_path.to_path_buf(),
            Some((
                wrapper_path.to_path_buf(),
                render_chain_wrapper(Path::new(o), our_script),
            )),
        ),
        None => (our_script.to_path_buf(), None),
    }
}

/// Remove our three vars (both hook vars + [`EVENT_LOG_VAR`]) from `.env`
/// content, preserving every other line — the inverse of [`rewrite_env`] for a
/// runner we installed *fresh* (its pre-install state was [`HookStatus::Unset`]).
/// Restoring a *chained* runner instead reuses [`rewrite_env`] (with `None` for
/// the event log) and the recovered originals. Pure.
pub(crate) fn remove_hook_vars(existing: &str) -> String {
    let kept: Vec<&str> = existing
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.starts_with(STARTED_VAR)
                && !t.starts_with(COMPLETED_VAR)
                && !t.starts_with(EVENT_LOG_VAR)
        })
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    let mut s = kept.join("\n");
    s.push('\n');
    s
}

/// Ensure `.env` sets `GHR_STATS_EVENT_LOG=<log>` exactly, touching ONLY that var
/// and leaving the hook vars untouched. Returns `Some(new_env)` when a change is
/// needed, `None` when it is already correct. This is the *repair* path for a
/// runner already wired to us (`HookStatus::Ours`) that predates the event-log
/// var — an upgrade from a version that installed hooks but never set the log
/// path would otherwise be skipped by `apply_hooks` and never emit events. Pure.
pub(crate) fn ensure_event_log(existing: &str, log: &Path) -> Option<String> {
    let want = log.display().to_string();
    if env_value(existing, EVENT_LOG_VAR).as_deref() == Some(want.as_str()) {
        return None; // already points at this runner's log — nothing to do
    }
    let mut out: Vec<String> = existing
        .lines()
        .filter(|l| !l.trim().starts_with(EVENT_LOG_VAR)) // drop any stale value
        .map(str::to_string)
        .collect();
    out.push(format!("{EVENT_LOG_VAR}={want}"));
    let mut s = out.join("\n");
    s.push('\n');
    Some(s)
}

/// Recover the operator's ORIGINAL hook path from a chain wrapper's text, so
/// uninstall can restore it. Reads the [`WRAP_MARKER`] provenance line written by
/// [`render_chain_wrapper`]; falls back to the first quoted path on the exec line
/// for any wrapper written before the marker existed. Pure.
pub(crate) fn original_from_wrapper(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix(WRAP_MARKER) {
            let p = rest.trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    // Pre-marker fallback: the first `"…"`-quoted token on a non-comment line
    // (the wrapper's exec line is `"<orig>" "$@"; rc=$?`).
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(inner) = t.split('"').nth(1)
            && !inner.is_empty()
        {
            return Some(PathBuf::from(inner));
        }
    }
    None
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
    fn classify_in_treats_any_scope_dir_as_ours() {
        // The cross-scope status bug: hooks install System-scope (they need
        // root), but the non-root TUI enumerates BOTH scope dirs. A System-scoped
        // hook — clean OR chained — must read as Ours even with the User dir also
        // a candidate. Checking only the euid's (User) dir mislabeled it Foreign.
        let sys = PathBuf::from("/var/lib/ghr-stats/hooks");
        let usr = PathBuf::from("/home/u/.local/share/ghr-stats/hooks");
        let dirs = [usr.clone(), sys.clone()];
        let clean = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                     ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        assert_eq!(classify_in(clean, &dirs), HookStatus::Ours);
        // Chained: `.env` points at the wrappers, which live inside our hooks dir.
        let chained = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/chain-r1-started.sh\n\
             ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/chain-r1-completed.sh\n";
        assert_eq!(classify_in(chained, &dirs), HookStatus::Ours);
        // Documents the pre-fix failure: against ONLY the User dir it reads Foreign.
        assert_eq!(
            classify_in(clean, std::slice::from_ref(&usr)),
            HookStatus::Foreign
        );
        // A genuinely foreign hook is still Foreign against both dirs.
        let foreign = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup.sh\n";
        assert_eq!(classify_in(foreign, &[usr, sys]), HookStatus::Foreign);
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
            Some(Path::new("/srv/actions-runner/runner-01/.ghr-stats-events.ndjson")),
        );
        assert!(out.contains("TMPDIR=/var/tmp/runner"));
        assert!(out.contains("KEEP=1"));
        assert!(!out.contains("/old/start.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh"));
        assert!(out.contains("ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh"));
        // Install wires the per-runner event-log var.
        assert!(out.contains(
            "GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson"
        ));
    }

    #[test]
    fn rewrite_env_none_strips_our_event_log_var() {
        // The restore path (chained uninstall) passes `None`: any GHR_STATS_EVENT_LOG
        // we injected must be stripped, never carried into the operator's .env.
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
    fn chain_wrapper_runs_both_and_preserves_rc() {
        let orig = Path::new("/usr/local/sbin/cleanup-started.sh");
        let w = render_chain_wrapper(orig, Path::new("/var/lib/ghr-stats/hooks/job-started.sh"));
        assert!(w.contains("/usr/local/sbin/cleanup-started.sh"));
        assert!(w.contains("/var/lib/ghr-stats/hooks/job-started.sh"));
        assert!(w.contains("exit \"$rc\""));
        // The provenance marker must be present AND recover the exact original,
        // or uninstall could strand the operator's hook. (Regression pin.)
        assert!(w.contains(WRAP_MARKER));
        assert_eq!(original_from_wrapper(&w).as_deref(), Some(orig));
    }

    #[test]
    fn plan_chain_slot_wraps_when_original_present_else_wires_our_script() {
        let our = Path::new("/var/lib/ghr-stats/hooks/job-started.sh");
        let wrapper = Path::new("/var/lib/ghr-stats/hooks/chain-r1-started.sh");
        // Original present → write a wrapper, wire the wrapper.
        let (target, w) = plan_chain_slot(Some("/opt/orig.sh"), our, wrapper);
        assert_eq!(target, wrapper);
        let (wp, content) = w.expect("a wrapper to write");
        assert_eq!(wp, wrapper);
        assert!(content.contains("/opt/orig.sh"));
        assert!(content.contains("job-started.sh"));
        // No original (the one-var-Foreign case) → wire our script directly, NO
        // wrapper (the bug was pointing the var at a wrapper never written).
        let (target, w) = plan_chain_slot(None, our, wrapper);
        assert_eq!(target, our);
        assert!(w.is_none(), "must not fabricate a wrapper with nothing to chain");
    }

    #[test]
    fn original_from_wrapper_reads_marker_then_falls_back() {
        // Marker present (the wrapper we render today).
        let w = render_chain_wrapper(
            Path::new("/opt/hooks/foreign.sh"),
            Path::new("/var/lib/ghr-stats/hooks/job-started.sh"),
        );
        assert_eq!(
            original_from_wrapper(&w).as_deref(),
            Some(Path::new("/opt/hooks/foreign.sh"))
        );
        // Pre-marker wrapper: recover from the first quoted exec token.
        let legacy =
            "#!/usr/bin/env bash\n# old\n\"/opt/hooks/foreign.sh\" \"$@\"; rc=$?\nexit \"$rc\"\n";
        assert_eq!(
            original_from_wrapper(legacy).as_deref(),
            Some(Path::new("/opt/hooks/foreign.sh"))
        );
        assert_eq!(original_from_wrapper("not a wrapper\n"), None);
    }

    #[test]
    fn remove_hook_vars_drops_all_three_and_preserves_others() {
        // Both hook vars AND our event-log var are stripped; other lines survive.
        let existing = "TMPDIR=/var/tmp/runner\n\
                        ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh\n\
                        KEEP=1\n\
                        GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson\n\
                        ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh\n";
        let out = remove_hook_vars(existing);
        assert_eq!(out, "TMPDIR=/var/tmp/runner\nKEEP=1\n");
        // An .env that was ONLY our three vars reverts to empty (true unset).
        let only = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/h/job-started.sh\n\
                    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/h/job-completed.sh\n\
                    GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson\n";
        assert_eq!(remove_hook_vars(only), "");
    }

    #[test]
    fn ensure_event_log_adds_when_missing_fixes_stale_noops_when_correct() {
        let log = Path::new("/srv/actions-runner/runner-01/.ghr-stats-events.ndjson");
        // Missing (the upgrade case: wired pre-fix, no event-log var) → add it,
        // leaving the hook vars untouched.
        let missing = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/chain-r1-started.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/chain-r1-completed.sh\n";
        let out = ensure_event_log(missing, log).expect("a change was needed");
        assert!(out.contains(
            "GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson"
        ));
        assert!(out.contains("chain-r1-started.sh")); // hook vars preserved
        // Stale value → replaced (exactly one line, the correct one).
        let stale = "GHR_STATS_EVENT_LOG=/old/path.ndjson\nKEEP=1\n";
        let fixed = ensure_event_log(stale, log).expect("a change was needed");
        assert!(!fixed.contains("/old/path.ndjson"));
        assert_eq!(fixed.matches("GHR_STATS_EVENT_LOG=").count(), 1);
        assert!(fixed.contains("KEEP=1"));
        // Already correct → no change.
        assert!(ensure_event_log(&out, log).is_none());
    }

    #[test]
    fn fresh_install_reverses_to_unset() {
        // unset (other lines only) → install → remove ⇒ byte-identical original,
        // even though install now also writes the per-runner event-log var.
        let original = "TMPDIR=/var/tmp/runner\nKEEP=1\n";
        let installed = rewrite_env(
            original,
            Path::new("/var/lib/ghr-stats/hooks/job-started.sh"),
            Path::new("/var/lib/ghr-stats/hooks/job-completed.sh"),
            Some(Path::new("/srv/actions-runner/runner-01/.ghr-stats-events.ndjson")),
        );
        assert_eq!(classify(&installed, &our()), HookStatus::Ours);
        assert!(installed.contains("GHR_STATS_EVENT_LOG="));
        assert_eq!(remove_hook_vars(&installed), original);
    }

    #[test]
    fn chained_install_reverses_to_original_foreign() {
        // A foreign runner (canonical STARTED-then-COMPLETED order) → chain → the
        // uninstall reversal must restore the exact original .env, byte-for-byte.
        let original = "TMPDIR=/var/tmp/runner\n\
                        ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup-started.sh\n\
                        ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup-completed.sh\n";
        // Install's chain step (mirrors ops::wizard::chain_for).
        let (orig_started, orig_completed) = current_hook_paths(original);
        let wrap_started = our().join("chain-runner-01-started.sh");
        let wrap_completed = our().join("chain-runner-01-completed.sh");
        let ws = render_chain_wrapper(Path::new(&orig_started.unwrap()), &wrap_started);
        let wc = render_chain_wrapper(Path::new(&orig_completed.unwrap()), &wrap_completed);
        let event_log = our().join("../runner-01/.ghr-stats-events.ndjson");
        let installed = rewrite_env(original, &wrap_started, &wrap_completed, Some(&event_log));
        assert_eq!(classify(&installed, &our()), HookStatus::Ours);

        // Uninstall reversal: recover the originals from the wrappers, restore.
        // `None` for the event log strips the var we injected, so the operator's
        // .env comes back byte-for-byte (no orphaned GHR_STATS_EVENT_LOG).
        let restored = rewrite_env(
            &installed,
            &original_from_wrapper(&ws).unwrap(),
            &original_from_wrapper(&wc).unwrap(),
            None,
        );
        assert_eq!(restored, original); // never stranded — the foreign hook is back
    }
}
