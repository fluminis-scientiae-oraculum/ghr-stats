//! Actions own a snapshot of their data, not a borrow of `App`: a refresh
//! reshuffles `app.runners` while the confirm popup is open.

use std::path::PathBuf;

use crate::shared::collectors::runners::{self, RunnerScratch, RunnerUnit};
use crate::shared::privileged::{self, Outcome, PrivilegedCall, UnitVerb};
use crate::tui::input::screen::Tty;

pub(crate) struct ConfirmPrompt {
    pub title: String,
    pub body: String,
    pub danger: bool,
}

pub(crate) enum ActionOutcome {
    Ok(String),
    Failed(String),
}

impl ActionOutcome {
    pub(crate) fn message(&self) -> String {
        match self {
            ActionOutcome::Ok(m) => format!("✓ {m}"),
            ActionOutcome::Failed(m) => format!("✗ {m}"),
        }
    }
}

/// Restarting reclaims the .NET runner's GC RAM.
pub(crate) struct RestartRunner {
    pub unit: RunnerUnit,
    pub agent_id: i64,
    /// Busy when armed: restarting cancels its job.
    pub busy: bool,
}

impl RestartRunner {
    /// Shared by `prompt` and `execute`, so the popup names the command that runs.
    fn call(&self) -> PrivilegedCall {
        PrivilegedCall::Systemctl {
            verb: UnitVerb::Restart,
            unit: self.unit.clone(),
        }
    }
}

/// Stop, empty `_temp` and `_diag` as the runner user, start. Idle only.
pub(crate) struct RecycleRunner {
    pub unit: RunnerUnit,
    pub agent_id: i64,
    pub install_dir: PathBuf,
    pub scratch: RunnerScratch,
}

impl RecycleRunner {
    fn recycle(&self) -> Result<(), String> {
        if !runners::is_idle_now(&self.install_dir) {
            return Err("runner is no longer idle; not recycled".to_string());
        }
        let steps = [
            ("stop", self.unit_call(UnitVerb::Stop)),
            (
                "purge _temp",
                PrivilegedCall::PurgeTemp(self.scratch.clone()),
            ),
            ("trim _diag", PrivilegedCall::TrimDiag(self.scratch.clone())),
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
            body: {
                let sudo = if privileged::is_root() { "" } else { "sudo " };
                if self.busy {
                    format!(
                        "{sudo}{}\nThe runner is busy: restarting cancels its job.",
                        self.call()
                    )
                } else {
                    format!("{sudo}{}\nReclaims the runner agent's GC RAM.", self.call())
                }
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
        ConfirmPrompt {
            title: format!("Recycle {} (#{})", self.unit, self.agent_id),
            body: format!(
                "stop · purge {temp} · trim {diag} · start\n\
                 (as the runner user; idle only)",
                temp = self.scratch.temp().display(),
                diag = self.scratch.diag().display()
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

/// The CLI's interactive hook detect/install flow, run on the real TTY.
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

/// Actions that suspend the TUI to run on the real terminal.
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
