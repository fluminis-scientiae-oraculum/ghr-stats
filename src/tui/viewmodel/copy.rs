use super::status::{GithubReason, JobsView, RunnerGithub};

pub(crate) const INSTALL_COLLECTOR: &str = "ghr-stats systemd install";
pub(crate) const ADD_PAT: &str = "add a read-only PAT on the Config tab [a]";
pub(crate) const INSTALL_HOOKS: &str =
    "install or chain it on the Config tab with [h] (as root), or run `sudo ghr-stats config`";

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

pub(crate) fn collecting_trends() -> String {
    format!(
        "Collecting… — trends fill as live samples arrive.\n\nInstall the collector for history \
         that persists across restarts:  {INSTALL_COLLECTOR}"
    )
}

pub(crate) fn collecting_sparkline() -> String {
    format!(
        "Collecting… — the sparkline fills as live samples arrive.\n\nInstall the collector for \
         history across restarts:  {INSTALL_COLLECTOR}"
    )
}

pub(crate) fn work_persistent_only() -> &'static str {
    "  Persistent only — install the collector to trend _work size"
}

pub(crate) fn version_warning(
    state: super::status::VersionState,
    ephemeral: Option<&crate::shared::ipc::client::EphemeralReason>,
) -> Option<String> {
    use super::status::VersionState;
    use crate::shared::ipc::client::EphemeralReason;

    // Wire drift first: it also explains why there is no collector data.
    if let Some(EphemeralReason::VersionDrift { server }) = ephemeral {
        return Some(format!(
            "A collector IS running but speaks IPC v{server} (this build speaks v{}). \
             Restart the service after upgrading:  sudo systemctl restart ghr-stats",
            crate::shared::ipc::VERSION
        ));
    }
    match state {
        VersionState::Drift | VersionState::CollectorUnknown => Some(
            "The running service is a different build than this binary — \
             restart it to pick up the upgrade:  sudo systemctl restart ghr-stats"
                .to_string(),
        ),
        VersionState::Match | VersionState::NoCollector => None,
    }
}
