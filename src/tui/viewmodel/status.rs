//! Presentation status derived from Model state — pure functions over primitive
//! inputs so the rules are testable in isolation and shared by every view.

use crate::shared::models::Mode;

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

/// How this binary's build relates to the collector's. Derived once here, pure
/// and testable, rather than compared inline at each render site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionState {
    /// No collector to compare against.
    NoCollector,
    /// A collector answered but reported no build version — it predates the
    /// field, which itself means it is an older binary.
    CollectorUnknown,
    /// Service and dashboard are the same build.
    Match,
    /// The running service is a DIFFERENT build than this binary. Almost always
    /// "upgraded the binary, forgot `systemctl restart`".
    Drift,
}

pub(crate) fn version_state(binary: &str, collector: Option<&str>, mode: Mode) -> VersionState {
    match (mode, collector) {
        (Mode::Ephemeral, _) => VersionState::NoCollector,
        (Mode::Persistent, None) => VersionState::CollectorUnknown,
        (Mode::Persistent, Some(v)) if v == binary => VersionState::Match,
        (Mode::Persistent, Some(_)) => VersionState::Drift,
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

    /// The upgrade-without-restart case is the one this exists for: same wire
    /// version (so the socket still connects) but a different build.
    #[test]
    fn version_state_flags_a_service_running_an_older_build() {
        assert_eq!(
            version_state("0.2.0", Some("0.2.0"), Mode::Persistent),
            VersionState::Match
        );
        assert_eq!(
            version_state("0.2.0", Some("0.1.4"), Mode::Persistent),
            VersionState::Drift
        );
        // A collector too old to report a version is itself an older build.
        assert_eq!(
            version_state("0.2.0", None, Mode::Persistent),
            VersionState::CollectorUnknown
        );
        // Nothing to compare against — not a drift, and must not warn.
        assert_eq!(
            version_state("0.2.0", None, Mode::Ephemeral),
            VersionState::NoCollector
        );
    }

    /// A version-drifted collector must not be reported as "no collector".
    #[test]
    fn version_warning_names_the_wire_mismatch_over_the_build_mismatch() {
        use crate::shared::ipc::client::EphemeralReason;
        use crate::tui::viewmodel::copy::version_warning;

        // Wire drift: the actionable one — it also explains the empty dashboard.
        let w = version_warning(
            VersionState::NoCollector,
            Some(EphemeralReason::VersionDrift { server: 8 }),
        )
        .expect("wire drift must warn");
        assert!(w.contains("IPC v8"));
        assert!(w.contains("systemctl restart"));

        // Genuinely no collector: silence, not a spurious upgrade nag.
        assert!(
            version_warning(
                VersionState::NoCollector,
                Some(EphemeralReason::NoCollector)
            )
            .is_none()
        );
        assert!(version_warning(VersionState::Match, None).is_none());
    }

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
