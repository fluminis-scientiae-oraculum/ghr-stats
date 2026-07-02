//! Presentation status derived from Model state — pure functions over primitive
//! inputs so the rules are testable in isolation and shared by every view.

use crate::tui::history::Mode;

/// Why the GitHub view has no data for the fleet. `github_reason` returning
/// `None` means data is present (render the counts / the runner's state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GithubReason {
    /// Ephemeral mode — GitHub is a collector-only feature (no network).
    EphemeralOnly,
    /// Persistent, but no read-only PAT is configured.
    NoPat,
    /// Persistent with a PAT, but the reconcile has returned nothing yet.
    ReconcilePending,
}

/// The fleet-level GitHub availability, most-specific cause first.
pub(crate) fn github_reason(
    mode: Mode,
    has_tokens: bool,
    reconcile_populated: bool,
) -> Option<GithubReason> {
    match mode {
        Mode::Ephemeral => Some(GithubReason::EphemeralOnly),
        Mode::Persistent if !has_tokens => Some(GithubReason::NoPat),
        Mode::Persistent if !reconcile_populated => Some(GithubReason::ReconcilePending),
        Mode::Persistent => None, // data present
    }
}

/// The reason a *specific* runner has no GitHub cell: the fleet reason, or —
/// when the reconcile has data but not for this runner — `NotSeen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunnerGithub {
    Reason(GithubReason),
    /// Reconcile returned rows, but none matched this runner's id.
    NotSeen,
}

/// Called only when the runner has no `ApiState`. If the fleet has data, this
/// runner simply wasn't in it (`NotSeen`); otherwise it's the fleet reason.
pub(crate) fn runner_github_absent(
    mode: Mode,
    has_tokens: bool,
    reconcile_populated: bool,
) -> RunnerGithub {
    match github_reason(mode, has_tokens, reconcile_populated) {
        Some(r) => RunnerGithub::Reason(r),
        None => RunnerGithub::NotSeen,
    }
}

/// Jobs-tab availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobsView {
    /// The ghr-stats hook is installed on `hooked` runners — awaiting jobs.
    Recording { hooked: usize },
    /// Persistent, but the hook feeds no runner yet.
    NoHooks,
    /// Ephemeral — jobs need the collector.
    EphemeralOnly,
}

pub(crate) fn jobs_view(mode: Mode, hooked_runners: usize) -> JobsView {
    match mode {
        Mode::Ephemeral => JobsView::EphemeralOnly,
        Mode::Persistent if hooked_runners > 0 => JobsView::Recording {
            hooked: hooked_runners,
        },
        Mode::Persistent => JobsView::NoHooks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_reason_covers_every_case_once() {
        // Ephemeral: always the collector, regardless of tokens/reconcile.
        assert_eq!(
            github_reason(Mode::Ephemeral, true, true),
            Some(GithubReason::EphemeralOnly)
        );
        // Persistent, no PAT.
        assert_eq!(
            github_reason(Mode::Persistent, false, false),
            Some(GithubReason::NoPat)
        );
        // Persistent, PAT set, reconcile empty.
        assert_eq!(
            github_reason(Mode::Persistent, true, false),
            Some(GithubReason::ReconcilePending)
        );
        // Persistent, PAT set, data present ⇒ available.
        assert_eq!(github_reason(Mode::Persistent, true, true), None);
    }

    #[test]
    fn runner_absent_is_not_seen_only_when_data_present() {
        // Data present but this runner missing ⇒ NotSeen.
        assert_eq!(
            runner_github_absent(Mode::Persistent, true, true),
            RunnerGithub::NotSeen
        );
        // No data ⇒ the fleet reason.
        assert_eq!(
            runner_github_absent(Mode::Persistent, false, false),
            RunnerGithub::Reason(GithubReason::NoPat)
        );
        assert_eq!(
            runner_github_absent(Mode::Ephemeral, true, true),
            RunnerGithub::Reason(GithubReason::EphemeralOnly)
        );
    }

    #[test]
    fn jobs_view_distinguishes_installed_from_absent() {
        assert_eq!(jobs_view(Mode::Ephemeral, 5), JobsView::EphemeralOnly);
        assert_eq!(jobs_view(Mode::Persistent, 0), JobsView::NoHooks);
        assert_eq!(
            jobs_view(Mode::Persistent, 3),
            JobsView::Recording { hooked: 3 }
        );
    }
}
