//! Canonical user-facing copy — the single home for the recurring calls-to-action
//! and the empty-state messages built from the [`super::status`] enums. Nothing
//! else in the TUI hardcodes these command strings.

use super::status::{GithubReason, JobsView, RunnerGithub};

/// Install the collector service (Ephemeral → Persistent).
pub(crate) const INSTALL_COLLECTOR: &str = "ghr-stats systemd install";
/// Add a read-only PAT via the Config tab.
pub(crate) const ADD_PAT: &str = "add a read-only PAT on the Config tab [a]";
/// Install or chain the runner job hook.
pub(crate) const INSTALL_HOOKS: &str =
    "install or chain it on the Config tab with [h] (as root), or run `sudo ghr-stats config`";

/// The Detail-panel GitHub cell text when there's no state for the runner.
pub(crate) fn runner_github_cell(rg: RunnerGithub) -> String {
    match rg {
        RunnerGithub::Reason(GithubReason::EphemeralOnly) => {
            "(Persistent only — needs the collector)".to_string()
        }
        RunnerGithub::Reason(GithubReason::NoPat) => {
            "(no PAT configured — add one on the Config tab [a])".to_string()
        }
        RunnerGithub::Reason(GithubReason::ReconcilePending) => {
            "(reconcile pending — or the PAT lacks access to this org)".to_string()
        }
        RunnerGithub::NotSeen => "(not reported by the GitHub API — org/PAT mismatch?)".to_string(),
    }
}

/// The Summary-line GitHub hint when the fleet view is absent.
pub(crate) fn github_summary_hint(reason: GithubReason) -> String {
    match reason {
        GithubReason::EphemeralOnly => {
            format!("Persistent only — install the collector (`{INSTALL_COLLECTOR}`)")
        }
        GithubReason::NoPat => ADD_PAT.to_string(),
        GithubReason::ReconcilePending => {
            "reconcile pending, or the PAT lacks org access".to_string()
        }
    }
}

/// The Jobs-tab empty-state body.
pub(crate) fn jobs_empty(view: JobsView) -> String {
    match view {
        JobsView::EphemeralOnly => format!(
            "Jobs are a Persistent-mode feature.\n\nInstall the collector to record job starts \
             and completions:  {INSTALL_COLLECTOR}"
        ),
        JobsView::Recording { hooked } => format!(
            "No jobs recorded yet.\n\nThe ghr-stats job hook is installed on {hooked} runner(s) — \
             starts and completions will appear here as runners pick up work."
        ),
        JobsView::NoHooks => format!(
            "No jobs recorded yet.\n\nThe ghr-stats job hook isn't feeding any runner yet. \
             {INSTALL_HOOKS}."
        ),
    }
}

/// The Trends-tab "still filling" empty state (Ephemeral rings before ~2 points).
pub(crate) fn collecting_trends() -> String {
    format!(
        "Collecting… — trends fill as live samples arrive.\n\nInstall the collector for history \
         that persists across restarts:  {INSTALL_COLLECTOR}"
    )
}

/// The Detail sparkline "still filling" empty state.
pub(crate) fn collecting_sparkline() -> String {
    format!(
        "Collecting… — the sparkline fills as live samples arrive.\n\nInstall the collector for \
         history across restarts:  {INSTALL_COLLECTOR}"
    )
}

/// The `_work` trend cell in Ephemeral mode (the expensive walk is collector-only).
pub(crate) fn work_persistent_only() -> &'static str {
    "  Persistent only — install the collector to trend _work size"
}
