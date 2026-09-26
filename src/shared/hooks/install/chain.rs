//! Chain wrappers: when an operator already owns a hook var, run their script (keeping its exit
//! code, the runner's pass/fail signal), then ours. [`original_from_wrapper`] must parse back what
//! [`render_chain_wrapper`] writes, or `uninstall` leaves the runner hookless.

use std::path::{Path, PathBuf};

/// Provenance line naming the original hook path, read back by `uninstall`.
const WRAP_MARKER: &str = "# ghr-stats-wraps:";

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

/// Returns the path to wire into `.env` and the wrapper to write, if any. No original ⇒ our script
/// directly, so a one-var `Foreign` runner never points at an unwritten wrapper. Pure.
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

/// Falls back to the first quoted exec-line path for wrappers written before [`WRAP_MARKER`].
pub(crate) fn original_from_wrapper(text: &str) -> Option<PathBuf> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix(WRAP_MARKER) {
            let p = rest.trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
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
    fn chain_wrapper_text_names_both_hooks_and_exits_rc() {
        let orig = Path::new("/usr/local/sbin/cleanup-started.sh");
        let w = render_chain_wrapper(orig, Path::new("/var/lib/ghr-stats/hooks/job-started.sh"));
        assert!(w.contains("/usr/local/sbin/cleanup-started.sh"));
        assert!(w.contains("/var/lib/ghr-stats/hooks/job-started.sh"));
        assert!(w.contains("exit \"$rc\""));
        assert!(w.contains(WRAP_MARKER));
        assert_eq!(original_from_wrapper(&w).as_deref(), Some(orig));
    }

    #[test]
    fn plan_chain_slot_wraps_when_original_present_else_wires_our_script() {
        let our = Path::new("/var/lib/ghr-stats/hooks/job-started.sh");
        let wrapper = Path::new("/var/lib/ghr-stats/hooks/chain-r1-started.sh");
        let (target, w) = plan_chain_slot(Some("/opt/orig.sh"), our, wrapper);
        assert_eq!(target, wrapper);
        let (wp, content) = w.expect("a wrapper to write");
        assert_eq!(wp, wrapper);
        assert!(content.contains("/opt/orig.sh"));
        assert!(content.contains("job-started.sh"));
        let (target, w) = plan_chain_slot(None, our, wrapper);
        assert_eq!(target, our);
        assert!(
            w.is_none(),
            "must not fabricate a wrapper with nothing to chain"
        );
    }

    #[test]
    fn original_from_wrapper_reads_marker_then_falls_back() {
        let w = render_chain_wrapper(
            Path::new("/opt/hooks/foreign.sh"),
            Path::new("/var/lib/ghr-stats/hooks/job-started.sh"),
        );
        assert_eq!(
            original_from_wrapper(&w).as_deref(),
            Some(Path::new("/opt/hooks/foreign.sh"))
        );
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
        let original = "TMPDIR=/var/tmp/runner\n\
                        ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/cleanup-started.sh\n\
                        ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/cleanup-completed.sh\n";
        let (orig_started, orig_completed) = current_hook_paths(original);
        let wrap_started = our().join("chain-runner-01-started.sh");
        let wrap_completed = our().join("chain-runner-01-completed.sh");
        let ws = render_chain_wrapper(Path::new(&orig_started.unwrap()), &wrap_started);
        let wc = render_chain_wrapper(Path::new(&orig_completed.unwrap()), &wrap_completed);
        let event_log = our().join("../runner-01/.ghr-stats-events.ndjson");
        let installed = rewrite_env(original, &wrap_started, &wrap_completed, Some(&event_log));
        assert_eq!(classify(&installed, &[our()]), HookStatus::Ours);

        // `None` strips the injected `GHR_STATS_EVENT_LOG`.
        let restored = rewrite_env(
            &installed,
            &original_from_wrapper(&ws).unwrap(),
            &original_from_wrapper(&wc).unwrap(),
            None,
        );
        assert_eq!(restored, original);
    }
}
