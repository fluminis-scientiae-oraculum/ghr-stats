//! Actions carried through the typestate (`screen`).
//!
//! Each action owns the data it will act on — an *owned snapshot*, not a borrow
//! of `App` (the 2 s refresh reshuffles `app.runners` while a confirm popup is
//! open, so a borrow would be a correctness bug). `execute` runs while the TUI
//! is suspended; privileged actions implement [`privileged::PrivilegedCall`]
//! and run through [`privileged::dispatch`], which enforces the declared
//! [`privileged::Needs`] before any shell-out (sudo when not root, on /dev/tty).

use std::path::PathBuf;

use crate::privileged::{self, Cleared, Needs, Outcome, PrivilegedCall};
use crate::tui::screen::Tty;

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

/// An action with a confirm prompt and an execution that runs on the real TTY.
/// Object-safe by construction (`&self`, no associated types).
pub(crate) trait Action {
    fn prompt(&self) -> ConfirmPrompt;
    fn execute(&self, tty: &mut Tty) -> ActionOutcome;
}

/// Bounce a runner's service to reclaim the .NET-runner GC RAM.
pub(crate) struct RestartRunner {
    pub unit: String,
    pub agent_id: i64,
}

/// Restart + purge the runner's OWN `_work/_temp` + trim `_diag` — idle-only,
/// scoped to its install dir from `.runner`, NEVER global `/tmp` or docker.
pub(crate) struct RecycleRunner {
    pub unit: String,
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
}

/// Launch the dialoguer config wizard on the real TTY.
pub(crate) struct AddOrg;

impl PrivilegedCall for RestartRunner {
    fn needs(&self) -> Needs {
        Needs::Sudo
    }
    fn perform(&self, cleared: &Cleared) -> Outcome {
        cleared.run(&["systemctl", "restart", &self.unit])
    }
}

impl Action for RestartRunner {
    fn prompt(&self) -> ConfirmPrompt {
        ConfirmPrompt {
            title: format!("Restart {} (#{})", self.unit, self.agent_id),
            body: format!(
                "sudo systemctl restart {}\nReclaims the runner agent's GC RAM.",
                self.unit
            ),
            danger: false,
        }
    }
    fn execute(&self, _tty: &mut Tty) -> ActionOutcome {
        match privileged::dispatch(self) {
            Outcome::Ok => ActionOutcome::Ok(format!("restarted {}", self.unit)),
            other => ActionOutcome::Failed(other.describe("restart")),
        }
    }
}

impl PrivilegedCall for RecycleRunner {
    fn needs(&self) -> Needs {
        Needs::Sudo
    }
    fn perform(&self, cleared: &Cleared) -> Outcome {
        let (temp, diag) = self.scoped_paths();
        let (temp_s, diag_s) = (temp.to_string_lossy(), diag.to_string_lossy());

        // Stop first; abort before touching anything if that fails.
        let stop = cleared.run(&["systemctl", "stop", &self.unit]);
        if !stop.is_ok() {
            return stop;
        }
        // Scoped purge — ONLY this runner's own dirs under its install dir.
        let _ = cleared.run(&["rm", "-rf", "--", &temp_s]);
        let _ = cleared.run(&["find", &diag_s, "-type", "f", "-delete"]);
        cleared.run(&["systemctl", "start", &self.unit])
    }
}

impl Action for RecycleRunner {
    fn prompt(&self) -> ConfirmPrompt {
        let (temp, diag) = self.scoped_paths();
        ConfirmPrompt {
            title: format!("Recycle {} (#{})", self.unit, self.agent_id),
            body: format!(
                "stop · purge {temp} · trim {diag} · start\n\
                 (scoped to THIS runner only — never global /tmp or docker; idle-only)",
                temp = temp.display(),
                diag = diag.display()
            ),
            danger: true,
        }
    }
    fn execute(&self, _tty: &mut Tty) -> ActionOutcome {
        match privileged::dispatch(self) {
            Outcome::Ok => ActionOutcome::Ok(format!("recycled {}", self.unit)),
            other => ActionOutcome::Failed(other.describe("recycle")),
        }
    }
}

impl Action for AddOrg {
    fn prompt(&self) -> ConfirmPrompt {
        ConfirmPrompt {
            title: "Configure (org / PAT / hooks)".to_string(),
            body: "Suspends the dashboard and runs the `ghr-stats config` wizard on this terminal."
                .to_string(),
            danger: false,
        }
    }
    fn execute(&self, _tty: &mut Tty) -> ActionOutcome {
        match crate::config_wizard::run(None) {
            Ok(()) => ActionOutcome::Ok("configuration updated".to_string()),
            Err(e) => ActionOutcome::Failed(e.to_string()),
        }
    }
}

/// Closed erasure of the action set for the loop's `ScreenState` — zero heap,
/// zero vtable, exhaustive. (`Box<dyn Action>` is a drop-in if it opens.)
pub(crate) enum ActionKind {
    Restart(RestartRunner),
    Recycle(RecycleRunner),
    AddOrg(AddOrg),
}

impl Action for ActionKind {
    fn prompt(&self) -> ConfirmPrompt {
        match self {
            ActionKind::Restart(a) => a.prompt(),
            ActionKind::Recycle(a) => a.prompt(),
            ActionKind::AddOrg(a) => a.prompt(),
        }
    }
    fn execute(&self, tty: &mut Tty) -> ActionOutcome {
        match self {
            ActionKind::Restart(a) => a.execute(tty),
            ActionKind::Recycle(a) => a.execute(tty),
            ActionKind::AddOrg(a) => a.execute(tty),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_scopes_temp_under_work_and_diag_at_install_root() {
        let r = RecycleRunner {
            unit: "x.service".to_string(),
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
