use crate::shared::ipc::client::Behind;
use crate::shared::models::Mode;

/// Why the fleet's GitHub view has no data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GithubReason {
    EphemeralOnly,
    NoPat,
    ReconcilePending,
}

pub(crate) fn github_reason(
    mode: Mode,
    has_tokens: bool,
    reconcile_populated: bool,
) -> Option<GithubReason> {
    match mode {
        Mode::Ephemeral => Some(GithubReason::EphemeralOnly),
        Mode::Persistent if !has_tokens => Some(GithubReason::NoPat),
        Mode::Persistent if !reconcile_populated => Some(GithubReason::ReconcilePending),
        Mode::Persistent => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunnerGithub {
    Reason(GithubReason),
    /// The reconcile returned rows, but none for this runner.
    NotSeen,
}

/// Only for a runner with no GitHub state.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionState {
    NoCollector,
    /// The collector predates the version field, so it is an older build.
    CollectorUnknown,
    Match,
    Drift(Option<Behind>),
}

pub(crate) fn version_state(binary: &str, collector: Option<&str>, mode: Mode) -> VersionState {
    match (mode, collector) {
        (Mode::Ephemeral, _) => VersionState::NoCollector,
        (Mode::Persistent, None) => VersionState::CollectorUnknown,
        (Mode::Persistent, Some(v)) if v == binary => VersionState::Match,
        (Mode::Persistent, Some(v)) => VersionState::Drift(Behind::of_builds(v, binary)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobsView {
    Recording { hooked: usize },
    NoHooks,
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
    fn version_state_flags_a_service_running_an_older_build() {
        assert_eq!(
            version_state("0.2.0", Some("0.2.0"), Mode::Persistent),
            VersionState::Match
        );
        assert_eq!(
            version_state("0.2.0", Some("0.1.4"), Mode::Persistent),
            VersionState::Drift(Some(Behind::Service))
        );
        assert_eq!(
            version_state("0.2.0", Some("0.10.0"), Mode::Persistent),
            VersionState::Drift(Some(Behind::Binary))
        );
        assert_eq!(
            version_state("0.2.0", None, Mode::Persistent),
            VersionState::CollectorUnknown
        );
        assert_eq!(
            version_state("0.2.0", None, Mode::Ephemeral),
            VersionState::NoCollector
        );
    }

    #[test]
    fn version_warning_names_the_wire_mismatch_over_the_build_mismatch() {
        use crate::shared::ipc::client::EphemeralReason;
        use crate::tui::viewmodel::copy::version_warning;

        let w = version_warning(
            VersionState::NoCollector,
            Some(&EphemeralReason::VersionDrift { server: 8 }),
        )
        .expect("wire drift must warn");
        assert!(w.contains("IPC v8"));
        assert!(w.contains("systemd install"));

        assert!(
            version_warning(
                VersionState::NoCollector,
                Some(&EphemeralReason::NoCollector)
            )
            .is_none()
        );
        assert!(version_warning(VersionState::Match, None).is_none());
    }

    #[test]
    fn github_reason_covers_every_case_once() {
        assert_eq!(
            github_reason(Mode::Ephemeral, true, true),
            Some(GithubReason::EphemeralOnly)
        );
        assert_eq!(
            github_reason(Mode::Persistent, false, false),
            Some(GithubReason::NoPat)
        );
        assert_eq!(
            github_reason(Mode::Persistent, true, false),
            Some(GithubReason::ReconcilePending)
        );
        assert_eq!(github_reason(Mode::Persistent, true, true), None);
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
