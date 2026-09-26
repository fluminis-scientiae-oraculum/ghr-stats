//! Actions carried through the typestate (`screen`).
//!
//! Each action owns the data it will act on — an *owned snapshot*, not a borrow
//! of `App` (the 2 s refresh reshuffles `app.runners` while a confirm popup is
//! open, so a borrow would be a correctness bug). `execute` runs while the TUI
//! is suspended; privileged actions shell out via [`privileged::run`], which
//! escalates per command (sudo when not root, prompting on /dev/tty). These
//! actions need no root *process* — see the two-tier model in `privileged`.
//!
//! Each privileged action builds its [`PrivilegedCall`] once and feeds it to
//! BOTH `prompt` and `execute`, so a confirm popup always names the command that
//! will actually run.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use crate::shared::collectors::runners::{self, RunnerUnit};
use crate::shared::privileged::{self, Outcome, PrivilegedCall, RunAs, UnitVerb};
use crate::tui::input::screen::Tty;

/// What the confirm popup shows for a pending action.
pub(crate) struct ConfirmPrompt {
    pub title: String,
    pub body: String,
    /// A destructive action — rendered in red.
    pub danger: bool,
}

/// The result of running an action while suspended.
pub(crate) enum ActionOutcome {
    Ok(String),
    Failed(String),
}

impl ActionOutcome {
    /// A short line for the status bar.
    pub(crate) fn message(&self) -> String {
        match self {
            ActionOutcome::Ok(m) => format!("✓ {m}"),
            ActionOutcome::Failed(m) => format!("✗ {m}"),
        }
    }
}

/// Bounce a runner's service to reclaim the .NET-runner GC RAM.
pub(crate) struct RestartRunner {
    pub unit: RunnerUnit,
    pub agent_id: i64,
    /// Busy when armed: restarting cancels its job.
    pub busy: bool,
}

impl RestartRunner {
    /// The one command this action runs. Both `prompt` and `execute` go through
    /// it, so the popup cannot advertise a command other than the one that runs.
    fn call(&self) -> PrivilegedCall {
        PrivilegedCall::Systemctl {
            verb: UnitVerb::Restart,
            unit: self.unit.clone(),
        }
    }
}

/// Stop, empty the runner's own `_temp` and `_diag` as the runner user, start.
/// Idle only, re-checked just before stopping.
pub(crate) struct RecycleRunner {
    pub unit: RunnerUnit,
    pub agent_id: i64,
    pub install_dir: PathBuf,
    pub work_folder: String,
}

impl RecycleRunner {
    /// The two dirs recycle reclaims, both scoped to THIS runner's install dir:
    /// `_temp` under the work folder, and `_diag` at the install ROOT (the runner
    /// writes its diagnostic logs to `<install>/_diag`, a sibling of the work
    /// folder — NOT inside it). Never global `/tmp`, never docker.
    fn scoped_paths(&self) -> (PathBuf, PathBuf) {
        let temp = self.install_dir.join(&self.work_folder).join("_temp");
        let diag = self.install_dir.join("_diag");
        (temp, diag)
    }

    /// stop → purge → start; the first failing step ends it.
    fn recycle(&self) -> Result<(), String> {
        if !runners::is_idle_now(&self.install_dir) {
            return Err("runner is no longer idle; not recycled".to_string());
        }
        let owner = std::fs::metadata(&self.install_dir)
            .map(|m| RunAs {
                uid: m.uid(),
                gid: m.gid(),
            })
            .map_err(|e| format!("{}: {e}", self.install_dir.display()))?;
        let (temp, diag) = self.scoped_paths();
        let steps = [
            ("stop", self.unit_call(UnitVerb::Stop)),
            ("purge _temp", PrivilegedCall::PurgeDir { dir: temp, owner }),
            (
                "trim _diag",
                PrivilegedCall::TrimFilesIn { dir: diag, owner },
            ),
            ("start", self.unit_call(UnitVerb::Start)),
        ];
        for (what, call) in steps {
            let out = privileged::run(&call);
            if !out.is_ok() {
                return Err(out.describe(what));
            }
        }
        Ok(())
    }

    fn unit_call(&self, verb: UnitVerb) -> PrivilegedCall {
        PrivilegedCall::Systemctl {
            verb,
            unit: self.unit.clone(),
        }
    }
}

impl RestartRunner {
    fn prompt(&self) -> ConfirmPrompt {
        ConfirmPrompt {
            title: format!("Restart {} (#{})", self.unit, self.agent_id),
            body: if self.busy {
                format!(
                    "sudo {}\nThe runner is busy: restarting cancels its job.",
                    self.call()
                )
            } else {
                format!("sudo {}\nReclaims the runner agent's GC RAM.", self.call())
            },
            danger: self.busy,
        }
    }
    fn execute(&self) -> ActionOutcome {
        match privileged::run(&self.call()) {
            Outcome::Ok => ActionOutcome::Ok(format!("restarted {}", self.unit)),
            other => ActionOutcome::Failed(other.describe("restart")),
        }
    }
}

impl RecycleRunner {
    fn prompt(&self) -> ConfirmPrompt {
        let (temp, diag) = self.scoped_paths();
        ConfirmPrompt {
            title: format!("Recycle {} (#{})", self.unit, self.agent_id),
            body: format!(
                "stop · purge {temp} · trim {diag} · start\n\
                 (as the runner user; idle only)",
                temp = temp.display(),
                diag = diag.display()
            ),
            danger: true,
        }
    }
    fn execute(&self) -> ActionOutcome {
        match self.recycle() {
            Ok(()) => ActionOutcome::Ok(format!("recycled {}", self.unit)),
            Err(why) => ActionOutcome::Failed(format!("recycle: {why}")),
        }
    }
}

/// Install / repair the runner job hooks (Config `[h]`). Runs the interactive
/// detect → install/chain/instruct flow on the real TTY while suspended, reusing
/// the CLI wizard's logic (one implementation). Root is checked BEFORE arming —
/// a non-root TUI gets an informational block instead of this action — and again
/// inside the flow.
pub(crate) struct InstallHooks {
    pub roots: Vec<PathBuf>,
}

impl InstallHooks {
    fn prompt(&self) -> ConfirmPrompt {
        ConfirmPrompt {
            title: "Install runner hooks".to_string(),
            body: "Detect each runner's job hooks and install/repair ours — chain or instruct \
                   for a foreign hook, never clobber. Runs on this terminal and may restart \
                   runners."
                .to_string(),
            danger: false,
        }
    }
    fn execute(&self) -> ActionOutcome {
        match crate::ops::configure::install_hooks_for_tui(&self.roots) {
            Ok(()) => ActionOutcome::Ok("hook install/repair finished (see terminal)".to_string()),
            Err(e) => ActionOutcome::Failed(e.to_string()),
        }
    }
}

/// Open the config file in `$EDITOR` (Config `[o]`), on the real TTY.
pub(crate) struct OpenConfig {
    pub path: PathBuf,
}

impl OpenConfig {
    fn prompt(&self) -> ConfirmPrompt {
        ConfirmPrompt {
            title: "Open config".to_string(),
            body: format!(
                "Open {} in $EDITOR (falls back to vi)?",
                self.path.display()
            ),
            danger: false,
        }
    }
    fn execute(&self) -> ActionOutcome {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        match std::process::Command::new(&editor).arg(&self.path).status() {
            Ok(s) if s.success() => ActionOutcome::Ok(format!("edited {}", self.path.display())),
            Ok(s) => {
                ActionOutcome::Failed(format!("{editor} exited with {}", s.code().unwrap_or(-1)))
            }
            Err(e) => ActionOutcome::Failed(format!("could not launch {editor}: {e}")),
        }
    }
}

/// The actions that suspend the TUI to run on the real terminal. Adding an org
/// and toggling metrics are not here: they run without leaving the dashboard.
pub(crate) enum ActionKind {
    Restart(RestartRunner),
    Recycle(RecycleRunner),
    InstallHooks(InstallHooks),
    OpenConfig(OpenConfig),
}

impl ActionKind {
    pub(crate) fn prompt(&self) -> ConfirmPrompt {
        match self {
            ActionKind::Restart(a) => a.prompt(),
            ActionKind::Recycle(a) => a.prompt(),
            ActionKind::InstallHooks(a) => a.prompt(),
            ActionKind::OpenConfig(a) => a.prompt(),
        }
    }

    /// Runs on the real terminal; the `Tty` token proves the TUI is suspended.
    pub(crate) fn execute(&self, _tty: &mut Tty) -> ActionOutcome {
        match self {
            ActionKind::Restart(a) => a.execute(),
            ActionKind::Recycle(a) => a.execute(),
            ActionKind::InstallHooks(a) => a.execute(),
            ActionKind::OpenConfig(a) => a.execute(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_scopes_temp_under_work_and_diag_at_install_root() {
        let r = RecycleRunner {
            unit: RunnerUnit::for_test("actions.runner.o.x.service"),
            agent_id: 1,
            install_dir: PathBuf::from("/srv/runners/r0"),
            work_folder: "_work".to_string(),
        };
        let (temp, diag) = r.scoped_paths();
        // `_temp` is under the work folder; `_diag` is at the install ROOT — a
        // live recycle on the fleet proved the runner writes <install>/_diag,
        // not <install>/_work/_diag (the original code trimmed the wrong path).
        assert_eq!(temp, PathBuf::from("/srv/runners/r0/_work/_temp"));
        assert_eq!(diag, PathBuf::from("/srv/runners/r0/_diag"));
        // Both stay under the install dir — never global /tmp, never docker.
        assert!(temp.starts_with(&r.install_dir));
        assert!(diag.starts_with(&r.install_dir));
    }
}
