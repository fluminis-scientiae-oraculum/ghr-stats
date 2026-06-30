//! Privileged host operations — the sudo model, expressed as a CONTRACT.
//!
//! This is the precedent for the project's "trait-as-contract" discipline:
//! define the obligation as a trait up front, so it cannot be forgotten or
//! re-derived ad hoc at each call site. Every privileged operation implements
//! [`PrivilegedCall`] and DECLARES its requirement via [`Needs`]. The only way
//! to actually shell out with privilege is a [`Cleared`] token, and the only
//! source of a `Cleared` is [`gate`] (or [`dispatch`], built on it) AFTER the
//! requirement is satisfied. So a new op that omits the gate has no `Cleared`,
//! cannot call [`Cleared::run`], and does not compile — the gate is enforced by
//! the type system, not by memory. Same capability-token discipline as the TUI
//! typestate (`Torn`/`Tty`), applied to privilege.
//!
//! `Cleared::run` runs the command directly when already root, else via `sudo`
//! (which prompts on `/dev/tty`, so call only while the TUI is *suspended* — the
//! typestate guarantees an action's `execute` runs inside the suspend window).

use std::process::Command;

/// The result of a privileged shell-out.
pub(crate) enum Outcome {
    Ok,
    /// The command ran but failed (exit code + first stderr line).
    Failed {
        code: Option<i32>,
        stderr: String,
    },
    /// The command could not be spawned at all (e.g. `sudo` not installed).
    Spawn(String),
    /// The call required a root *process* and we were not root — carries the
    /// `sudo <abs> <cmd>` hint to re-run.
    NeedsRoot {
        hint: String,
    },
}

impl Outcome {
    pub(crate) fn is_ok(&self) -> bool {
        matches!(self, Outcome::Ok)
    }

    /// A short, actionable line describing the result of `what`.
    pub(crate) fn describe(&self, what: &str) -> String {
        match self {
            Outcome::Ok => format!("{what}: done"),
            Outcome::Failed { code, stderr } => {
                let detail = if stderr.is_empty() {
                    code.map(|c| format!("exit {c}"))
                        .unwrap_or_else(|| "failed".to_string())
                } else {
                    stderr.clone()
                };
                format!("{what}: {detail}")
            }
            Outcome::Spawn(e) => format!("{what}: could not run ({e}) — is `sudo` installed?"),
            Outcome::NeedsRoot { hint } => format!("{what}: needs root — re-run `{hint}`"),
        }
    }
}

/// What a privileged call needs from the process to run correctly. Declared by
/// each [`PrivilegedCall`] so the requirement lives at the type, not in a
/// remembered `is_root()` check at the call site.
pub(crate) enum Needs {
    /// The leaf command self-escalates: run directly if root, else via `sudo`.
    /// Correctness does NOT depend on the program's own scope — e.g. `systemctl
    /// restart`. Works whether or not we started as root.
    Sudo,
    /// The PROCESS must already be root, because the op depends on its
    /// euid-derived [`Scope`](crate::paths::Scope) being `System` (writing hook
    /// scripts to a runner-readable system path, or `/etc`). `sudo` on a leaf
    /// command cannot relocate our scope, so a non-root process is refused with
    /// a re-run hint for `resume` (the subcommand to re-run under sudo).
    RootProcess { resume: &'static str },
}

/// The contract every privileged operation implements. The requirement is
/// declared by `needs`; the work is `perform`, which can only run with a
/// [`Cleared`] — so it cannot execute un-gated. Object-safe (`&self`, no
/// associated types) so the loop's erased actions flow through it.
pub(crate) trait PrivilegedCall {
    fn needs(&self) -> Needs;
    /// Do the work, using `cleared` for every privileged shell-out. Only
    /// reachable with a `Cleared`, which [`dispatch`] mints after the gate.
    fn perform(&self, cleared: &Cleared) -> Outcome;
}

/// Proof the privilege gate was cleared. The private field makes it
/// un-fabricable from outside this module; the only mint sites are [`gate`] and
/// [`dispatch`], both AFTER [`Needs`] is satisfied. A privileged shell-out goes
/// through [`Cleared::run`] and nothing else (the raw runner is private), so
/// privilege can only flow from a cleared gate.
pub(crate) struct Cleared(());

impl Cleared {
    /// Run a privileged command — directly if root, else via `sudo`. `argv[0]`
    /// is the program.
    pub(crate) fn run(&self, argv: &[&str]) -> Outcome {
        run_raw(argv)
    }
}

/// Clear the gate for `needs`, returning the capability token — or `Err(hint)`
/// when the process must be root and is not. The core primitive: [`dispatch`]
/// wraps it for one-shot calls, while an interactive multi-step flow (the hook
/// wizard) holds the returned `Cleared` across its steps.
pub(crate) fn gate(needs: Needs) -> Result<Cleared, String> {
    match needs {
        Needs::RootProcess { resume } if !is_root() => Err(sudo_hint(resume)),
        _ => Ok(Cleared(())),
    }
}

/// Execute a self-contained privileged call: enforce `needs()`, then `perform`.
/// The single entry point for one-shot ops — there is no other way to run one.
pub(crate) fn dispatch(call: &dyn PrivilegedCall) -> Outcome {
    match gate(call.needs()) {
        Ok(cleared) => call.perform(&cleared),
        Err(hint) => Outcome::NeedsRoot { hint },
    }
}

/// Whether we are already running as root.
pub(crate) fn is_root() -> bool {
    uzers::get_effective_uid() == 0
}

/// A "re-run me as root" hint carrying the binary's ABSOLUTE path — so it works
/// even when ghr-stats was `cargo install`ed to `~/.cargo/bin`, which is not on
/// sudo's `secure_path`. Falls back to the bare name if the path is unknown.
pub(crate) fn sudo_hint(subcommand: &str) -> String {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "ghr-stats".to_string());
    format!("sudo {exe} {subcommand}")
}

/// The raw shell-out. PRIVATE on purpose: privilege only flows through a
/// [`Cleared`], so this can't be called without clearing the gate first.
fn run_raw(argv: &[&str]) -> Outcome {
    if argv.is_empty() {
        return Outcome::Spawn("empty command".to_string());
    }
    let (program, rest): (&str, Vec<&str>) = if is_root() {
        (argv[0], argv[1..].to_vec())
    } else {
        ("sudo", argv.to_vec())
    };
    match Command::new(program).args(&rest).output() {
        Ok(o) if o.status.success() => Outcome::Ok,
        Ok(o) => Outcome::Failed {
            code: o.status.code(),
            stderr: first_line(&o.stderr),
        },
        Err(e) => Outcome::Spawn(e.to_string()),
    }
}

/// The first non-empty line of captured stderr, trimmed.
fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_picks_first_nonempty() {
        assert_eq!(first_line(b"\n  boom: nope \nmore\n"), "boom: nope");
        assert_eq!(first_line(b""), "");
    }

    #[test]
    fn describe_is_actionable() {
        assert_eq!(Outcome::Ok.describe("restart"), "restart: done");
        assert_eq!(
            Outcome::Failed {
                code: Some(1),
                stderr: "Unit not found".into()
            }
            .describe("restart"),
            "restart: Unit not found"
        );
        assert!(
            Outcome::Spawn("x".into())
                .describe("restart")
                .contains("sudo")
        );
    }

    #[test]
    fn sudo_calls_always_clear_the_gate() {
        // A `Sudo` requirement never needs the process to be root, so the gate
        // always mints a `Cleared` (the leaf command self-escalates).
        assert!(gate(Needs::Sudo).is_ok());
    }

    #[test]
    fn needs_root_outcome_carries_the_rerun_hint() {
        let o = Outcome::NeedsRoot {
            hint: "sudo /opt/ghr-stats config".into(),
        };
        let msg = o.describe("hooks");
        assert!(msg.contains("needs root"));
        assert!(msg.contains("sudo /opt/ghr-stats config"));
    }
}
