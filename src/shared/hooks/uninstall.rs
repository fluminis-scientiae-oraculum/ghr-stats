//! Runner hook REVERSAL — the inverse of `install`, and just as careful.
//!
//! Install never clobbers a foreign hook; uninstall must never *strand* one.
//! Per runner we re-detect from the authoritative source (its live `.env` + our
//! on-disk wrappers) and act only on what we ourselves installed:
//!
//! - **fresh** (our plain `job-*.sh`, runner was unset) → strip the two vars;
//! - **chained** (our `chain-*.sh` wrapper, runner had a foreign hook) → restore
//!   the operator's ORIGINAL hook (recovered from the wrapper) + delete the
//!   wrapper — the runner is left exactly as we found it;
//! - **foreign / unset / mixed / unreadable** → leave it untouched, report why.
//!
//! No manifest: the classification comes from the same authoritative per-runner
//! sources install used, so a hand-edited `.env` self-corrects instead of the
//! reversal acting on a stale record.

use std::path::{Path, PathBuf};

use crate::shared::hooks::env::{self, EnvFile};
use crate::shared::hooks::install::{self, HookStatus};
use crate::shared::models::RunnerInfo;

/// What a runner's `.env` reveals about *our* footprint on it. Pure result of
/// [`classify_revert`]; the paths carried by `Chained` are the wrapper scripts to
/// read + delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevertKind {
    /// Foreign hook or unset — not ours; never touch it.
    NotManaged,
    /// Both vars are our plain scripts — strip them (back to the unset state).
    Fresh,
    /// Both vars are our chain wrappers — restore the wrapped originals.
    Chained {
        started_wrapper: PathBuf,
        completed_wrapper: PathBuf,
    },
    /// Managed by us but in a mixed/partial state — report, never auto-mutate.
    Mixed,
}

/// Classify how (if at all) to revert a runner from its `.env` text + our hooks
/// dir. Pure. Only a state where BOTH vars point at our scripts is ever touched.
pub(crate) fn classify_revert(env: &str, our_dir: &Path) -> RevertKind {
    let inside =
        |p: &Path| install::is_directly_in(p, std::slice::from_ref(&our_dir.to_path_buf()));
    let is_chain =
        |p: &Path| file_name_str(p).is_some_and(|n| n.starts_with("chain-") && n.ends_with(".sh"));
    let is_fresh = |p: &Path| {
        matches!(
            file_name_str(p),
            Some("job-started.sh" | "job-completed.sh")
        )
    };

    match install::current_hook_paths(env) {
        (None, None) => RevertKind::NotManaged,
        (Some(s), Some(c)) => {
            let (sp, cp) = (PathBuf::from(s), PathBuf::from(c));
            if !(inside(&sp) && inside(&cp)) {
                return RevertKind::NotManaged; // at least one is a foreign hook
            }
            if is_chain(&sp) && is_chain(&cp) {
                RevertKind::Chained {
                    started_wrapper: sp,
                    completed_wrapper: cp,
                }
            } else if is_fresh(&sp) && is_fresh(&cp) {
                RevertKind::Fresh
            } else {
                RevertKind::Mixed
            }
        }
        // Exactly one var set: ours ⇒ an odd half-install (manual); else foreign.
        (s, c) => match s.or(c).map(PathBuf::from) {
            Some(p) if inside(&p) => RevertKind::Mixed,
            _ => RevertKind::NotManaged,
        },
    }
}

/// The concrete, previewable action for one runner — built by [`plan_runner`]
/// without mutating anything, so the dry-run shows exactly what execution does.
#[derive(Debug, Clone)]
pub(crate) enum RevertAction {
    /// Nothing to do; `why` explains (foreign / unset / unreadable).
    Leave { why: String },
    /// Ours but ambiguous — needs a human; `why` explains.
    Manual { why: String },
    /// Fresh install: rewrite `.env` to `new_env` (the two vars removed).
    Strip { new_env: String },
    /// Chained: rewrite `.env` to `new_env` (originals restored) + delete the
    /// wrappers; `originals` is shown in the plan so the operator sees what returns.
    Restore {
        new_env: String,
        originals: (PathBuf, PathBuf),
        wrappers: Vec<PathBuf>,
    },
}

/// A per-runner reversal plan (no mutation performed).
pub(crate) struct RunnerHookPlan {
    pub name: String,
    pub dir: PathBuf,
    env: Option<EnvFile>,
    pub action: RevertAction,
}

/// Build the reversal plan for one runner by reading its live `.env` (+ any of
/// our wrappers it points at). No mutation — safe to call for the dry-run.
pub(crate) fn plan_runner(r: &RunnerInfo, our_dir: &Path) -> RunnerHookPlan {
    let (env, action) = match env::read(&r.dir) {
        Err(e) => (
            None,
            RevertAction::Leave {
                why: format!(".env unreadable ({e})"),
            },
        ),
        Ok(env) => {
            let action = plan_action(&env.text, our_dir);
            (Some(env), action)
        }
    };
    RunnerHookPlan {
        name: r.name.clone(),
        dir: r.dir.clone(),
        env,
        action,
    }
}

/// The action half of [`plan_runner`], separated so the classification + restore
/// arithmetic is unit-testable against on-disk wrapper files. Reads wrappers (to
/// recover originals) but writes nothing.
fn plan_action(text: &str, our_dir: &Path) -> RevertAction {
    match classify_revert(text, our_dir) {
        RevertKind::NotManaged => RevertAction::Leave {
            why: match install::classify(text, our_dir) {
                HookStatus::Unset => "no ghr-stats hook (unset)".to_string(),
                _ => "foreign hook — left untouched (not ours)".to_string(),
            },
        },
        RevertKind::Mixed => RevertAction::Manual {
            why: "partially ghr-stats-managed .env — review by hand".to_string(),
        },
        RevertKind::Fresh => RevertAction::Strip {
            new_env: install::remove_hook_vars(text),
        },
        RevertKind::Chained {
            started_wrapper,
            completed_wrapper,
        } => {
            let os = read_wrapped_original(&started_wrapper);
            let oc = read_wrapped_original(&completed_wrapper);
            match (os, oc) {
                (Some(os), Some(oc)) => RevertAction::Restore {
                    // `None`: strip the GHR_STATS_EVENT_LOG we injected — never
                    // carry our var into the restored foreign `.env`.
                    new_env: install::rewrite_env(text, &os, &oc, None),
                    originals: (os, oc),
                    wrappers: vec![started_wrapper, completed_wrapper],
                },
                _ => RevertAction::Manual {
                    why: "chain wrapper missing its original-hook marker — restore by hand"
                        .to_string(),
                },
            }
        }
    }
}

/// Read a wrapper file and recover the operator's original hook path from it.
fn read_wrapped_original(wrapper: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(wrapper).ok()?;
    install::original_from_wrapper(&text)
}

/// Apply a runner's reversal plan (privileged); returns a receipt line.
pub(crate) fn apply_runner(plan: &RunnerHookPlan) -> String {
    let (new_env, restored) = match &plan.action {
        RevertAction::Leave { why } => return format!("  · {} — {why}", plan.name),
        RevertAction::Manual { why } => return format!("  ⚠ {} — {why}", plan.name),
        RevertAction::Strip { new_env } => (new_env, None),
        RevertAction::Restore {
            new_env,
            originals,
            wrappers,
        } => (new_env, Some((originals, wrappers))),
    };
    let Some(env) = &plan.env else {
        return format!("  · {} — .env unreadable", plan.name);
    };
    let out = env::write_env_as_root(env, new_env);
    if !out.is_ok() {
        return format!("  ✗ {} — {}", plan.name, out.describe("write .env"));
    }
    let done = match restored {
        None => "hook removed".to_string(),
        Some((originals, wrappers)) => {
            for w in wrappers {
                let _ = std::fs::remove_file(w);
            }
            format!("restored your hook ({})", originals.0.display())
        }
    };
    format!(
        "  ✓ {} — {done}{}",
        plan.name,
        env::restart_if_idle(&plan.dir)
    )
}

fn file_name_str(p: &Path) -> Option<&str> {
    p.file_name().and_then(|n| n.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::hooks::install::chain::render_chain_wrapper;
    use std::io::Write;

    fn our() -> PathBuf {
        PathBuf::from("/var/lib/ghr-stats/hooks")
    }

    #[test]
    fn classify_revert_distinguishes_fresh_chained_foreign() {
        let unset = "";
        assert_eq!(classify_revert(unset, &our()), RevertKind::NotManaged);

        let foreign = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/usr/local/sbin/x.sh\n\
                       ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/y.sh\n";
        assert_eq!(classify_revert(foreign, &our()), RevertKind::NotManaged);

        let fresh = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                     ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        assert_eq!(classify_revert(fresh, &our()), RevertKind::Fresh);

        let chained = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/chain-r1-started.sh\n\
             ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/chain-r1-completed.sh\n";
        assert_eq!(
            classify_revert(chained, &our()),
            RevertKind::Chained {
                started_wrapper: our().join("chain-r1-started.sh"),
                completed_wrapper: our().join("chain-r1-completed.sh"),
            }
        );

        // One ours, one foreign ⇒ never a clean revert.
        let half = "ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                    ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/usr/local/sbin/y.sh\n";
        assert_eq!(classify_revert(half, &our()), RevertKind::NotManaged);
    }

    #[test]
    fn plan_action_restore_recovers_original_from_disk_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let our_dir = dir.path();
        // Lay down two real chain wrappers pointing at the operator's hooks.
        let orig_s = "/usr/local/sbin/cleanup-started.sh";
        let orig_c = "/usr/local/sbin/cleanup-completed.sh";
        for (name, orig) in [
            ("chain-r1-started.sh", orig_s),
            ("chain-r1-completed.sh", orig_c),
        ] {
            let mut f = std::fs::File::create(our_dir.join(name)).unwrap();
            f.write_all(render_chain_wrapper(Path::new(orig), our_dir).as_bytes())
                .unwrap();
        }
        let env = format!(
            "TMPDIR=/x\n\
             ACTIONS_RUNNER_HOOK_JOB_STARTED={}\n\
             ACTIONS_RUNNER_HOOK_JOB_COMPLETED={}\n",
            our_dir.join("chain-r1-started.sh").display(),
            our_dir.join("chain-r1-completed.sh").display(),
        );
        match plan_action(&env, our_dir) {
            RevertAction::Restore {
                new_env,
                originals,
                wrappers,
            } => {
                assert_eq!(originals.0, PathBuf::from(orig_s));
                assert_eq!(originals.1, PathBuf::from(orig_c));
                assert!(new_env.contains(&format!("ACTIONS_RUNNER_HOOK_JOB_STARTED={orig_s}")));
                assert!(new_env.contains("TMPDIR=/x")); // untouched line preserved
                assert!(!new_env.contains("chain-r1")); // our wrappers gone from .env
                assert_eq!(wrappers.len(), 2);
            }
            other => panic!("expected Restore, got {other:?}"),
        }
    }

    #[test]
    fn plan_action_fresh_strips_and_foreign_leaves() {
        let our_dir = Path::new("/var/lib/ghr-stats/hooks");
        let fresh = "KEEP=1\n\
                     ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh\n\
                     ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh\n";
        match plan_action(fresh, our_dir) {
            RevertAction::Strip { new_env } => assert_eq!(new_env, "KEEP=1\n"),
            other => panic!("expected Strip, got {other:?}"),
        }
        match plan_action(
            "ACTIONS_RUNNER_HOOK_JOB_STARTED=/opt/x.sh\nACTIONS_RUNNER_HOOK_JOB_COMPLETED=/opt/y.sh\n",
            our_dir,
        ) {
            RevertAction::Leave { .. } => {}
            other => panic!("expected Leave, got {other:?}"),
        }
    }
}
