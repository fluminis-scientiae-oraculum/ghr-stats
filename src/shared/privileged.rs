//! Privileged host operations — the sudo model as a template-method CONTRACT.
//!
//! The project's trait-as-contract precedent, kept deliberately simple (KISS).
//! [`PrivilegedExecution::do_execute`] is the entry point AND the template: it
//! always clears the gate ([`ensure`](PrivilegedExecution::ensure)) before
//! running the work ([`contract`](PrivilegedExecution::contract)). An op writes
//! only `contract` (and overrides `ensure` if it needs a root *process*) — so
//! the gate cannot be forgotten: it lives once, in the default `do_execute`, and
//! that is how every privileged op is run. No capability token, no separate
//! dispatcher — the default method *is* the enforcement.
//!
//! Commands run directly when already root, else via `sudo` (which prompts on
//! `/dev/tty`, so call only while the TUI is *suspended* — the typestate
//! guarantees an action's `execute` runs inside the suspend window).

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
    /// `ensure` refused: this op needs a root *process* and we were not root —
    /// carries the `sudo <abs> <cmd>` hint to re-run.
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

/// A privileged operation, as a template-method contract.
///
/// - [`contract`](Self::contract) — the work. The only required method; shell
///   out via [`run`] (which sudo-wraps when not root).
/// - [`ensure`](Self::ensure) — the gate. DEFAULT = no extra requirement, since
///   `contract`'s commands self-escalate via sudo. Override to require a root
///   *process* when correctness depends on `Scope::System` (see [`require_root`]).
/// - [`do_execute`](Self::do_execute) — the entry point and template: `ensure`
///   then `contract`. It is the only way to run the op, so the gate can't be
///   skipped.
pub(crate) trait PrivilegedExecution {
    fn contract(&self) -> Outcome;

    fn ensure(&self) -> Result<(), String> {
        Ok(())
    }

    fn do_execute(&self) -> Outcome {
        match self.ensure() {
            Ok(()) => self.contract(),
            Err(hint) => Outcome::NeedsRoot { hint },
        }
    }
}

/// Require a root *process*, or the absolute-path re-run hint for `resume`. For
/// `ensure` overrides and for interactive flows (the hook wizard) that gate once
/// then do privileged work across several steps.
pub(crate) fn require_root(resume: &'static str) -> Result<(), String> {
    if is_root() {
        Ok(())
    } else {
        Err(sudo_hint(resume))
    }
}

/// Run a privileged command — directly if root, else via `sudo`. `argv[0]` is
/// the program. Called by a `contract` (after `do_execute` cleared the gate) or
/// by a gated interactive flow.
pub(crate) fn run(argv: &[&str]) -> Outcome {
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

/// Whether we are already running as root.
pub(crate) fn is_root() -> bool {
    uzers::get_effective_uid() == 0
}

/// This binary's ABSOLUTE path — the basis for every "re-run as root" hint, so
/// they work even when ghr-stats was `cargo install`ed to `~/.cargo/bin` (which
/// is NOT on sudo's `secure_path`). Falls back to the bare name if unknown.
pub(crate) fn exe_path() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "ghr-stats".to_string())
}

/// A "re-run me as root" hint carrying the binary's absolute path.
pub(crate) fn sudo_hint(subcommand: &str) -> String {
    format!("sudo {} {subcommand}", exe_path())
        .trim_end()
        .to_string()
}

/// Guidance for running the whole tool as root, spelling out the sudo
/// `secure_path` gap that bites a user-wide install. Shown (as an informational
/// block, never an error) when a root-only action is invoked from a non-root
/// TUI, and in the help sheet. The gate informs; it does not fail.
pub(crate) fn root_guidance() -> String {
    format!(
        "Installing runner hooks edits each runner's root-owned .env and writes shared \
         scripts, so the whole process must run as root.\n\n\
         Re-run the dashboard as root:\n\
         \x20\x20sudo {exe}\n\n\
         If `sudo ghr-stats` says \"command not found\", that is expected: sudo resets PATH to a \
         secure default that excludes ~/.cargo/bin and ~/.local/bin, so a user-wide install is \
         not on it. Use the absolute path above, or install system-wide with\n\
         \x20\x20{exe} systemd install --system\n\
         which copies the binary to /usr/local/bin (on sudo's path).",
        exe = exe_path()
    )
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

    /// A no-op op whose `contract` does not shell out, so the template can be
    /// exercised without a real command.
    struct Probe {
        gate: Result<(), String>,
    }
    impl PrivilegedExecution for Probe {
        fn contract(&self) -> Outcome {
            Outcome::Ok
        }
        fn ensure(&self) -> Result<(), String> {
            self.gate.clone()
        }
    }

    #[test]
    fn do_execute_runs_contract_only_after_the_gate_passes() {
        // gate passes → contract runs.
        assert!(Probe { gate: Ok(()) }.do_execute().is_ok());
        // gate refuses → NeedsRoot, contract never runs, hint carried through.
        let refused = Probe {
            gate: Err("sudo /opt/ghr-stats config".into()),
        }
        .do_execute();
        assert!(matches!(refused, Outcome::NeedsRoot { .. }));
        assert!(
            refused
                .describe("hooks")
                .contains("sudo /opt/ghr-stats config")
        );
    }

    #[test]
    fn default_ensure_needs_no_root() {
        // The default gate (no override) imposes no extra requirement.
        struct Plain;
        impl PrivilegedExecution for Plain {
            fn contract(&self) -> Outcome {
                Outcome::Ok
            }
        }
        assert!(Plain.ensure().is_ok());
    }
}
