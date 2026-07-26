//! The chain-wrapper FORMAT: the one thing this module writes that something
//! else has to read back.
//!
//! When an operator already owns a runner's hook var, we never overwrite it — we
//! write a wrapper that runs their script (preserving its exit code, which is the
//! runner's pass/fail signal) and then appends our event. That wrapper carries a
//! [`WRAP_MARKER`] provenance line naming the ORIGINAL path.
//!
//! Those three functions are together because they are two halves of one
//! contract. [`render_chain_wrapper`] writes the marker;
//! [`original_from_wrapper`] parses it back, for `uninstall`. If they drift, a
//! reversal cannot recover the operator's script and the runner is left HOOKLESS
//! — the exact inverse of never-clobber, and a worse outcome than never having
//! chained at all. Keeping the writer and its reader in one file is what makes
//! that pair reviewable as a pair; the round-trip test at the bottom is the
//! executable form of the same statement.
//!
//! [`original_from_wrapper`] also falls back to the first quoted path on the exec
//! line, which is how a wrapper written BEFORE the marker existed still reverses.
//! That fallback is why the marker could be introduced without stranding anyone.
//!
//! [`plan_chain_slot`] is here rather than with the installer because it decides
//! whether a wrapper exists at all for a given slot. It is what makes chaining a
//! `Foreign` runner with only ONE of the two hook vars set safe: the empty slot
//! gets our script directly instead of being pointed at a wrapper nobody wrote.

use std::path::{Path, PathBuf};

/// Provenance marker written into every chain wrapper so `uninstall` can recover
/// the operator's ORIGINAL hook path unambiguously (a stable comment, not a
/// re-parse of the exec line). Reversing a chained runner must restore this exact
/// path — never leave the runner hookless (the inverse of never-clobber).
const WRAP_MARKER: &str = "# ghr-stats-wraps:";

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

#[cfg(test)]
mod tests {
    use super::super::fixtures::our;
    use super::super::{HookStatus, classify, current_hook_paths, rewrite_env};
    use super::*;

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
        assert!(
            w.is_none(),
            "must not fabricate a wrapper with nothing to chain"
        );
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
