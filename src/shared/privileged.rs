//! Privileged host operations. Two distinct needs, deliberately not unified:
//!
//! 1. **Per-command escalation** — [`run`] executes directly when already root,
//!    else via `sudo`. Enough whenever each command can escalate on its own
//!    (`systemctl restart`, `install(1)`). `sudo` prompts on `/dev/tty`, so call
//!    only while the TUI is *suspended* — the typestate guarantees an action's
//!    `execute` runs inside the suspend window.
//! 2. **A root *process*** — [`require_root`] / [`is_root`], for work whose
//!    correctness depends on the process itself being root: it writes across
//!    scopes (`/etc`, `/usr/local/bin`, root-owned runner `.env` files) or must
//!    resolve our own install scope. Per-op `sudo` cannot supply this. Gated at
//!    the three entry points that need it — `ops::systemd::install`,
//!    `ops::wizard::apply_hooks`, `ops::uninstall`.
//!
//! These are free functions on purpose. A `PrivilegedExecution` template-method
//! trait lived here until 0.2.1 and was removed: it wrapped only the two TUI
//! actions, which need (1) and never overrode the gate, while all four sites
//! needing (2) called the free functions directly — so it advertised an
//! enforcement it did not provide. Re-adding one is only worth it if it actually
//! covers the `ops::` gates; see the backlog entry for D1.

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
        }
    }
}

/// Require a root *process*, or the absolute-path re-run hint for `resume`. For
/// the flows that gate once then do privileged work across several steps (the
/// hook wizard, `systemd install --system`, system-scope `uninstall`).
pub(crate) fn require_root(resume: &'static str) -> Result<(), String> {
    if is_root() {
        Ok(())
    } else {
        Err(sudo_hint(resume))
    }
}

/// Run a privileged command — directly if root, else via `sudo`. `argv[0]` is
/// the program.
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

    #[test]
    fn sudo_hint_carries_an_absolute_path() {
        // The whole point of the hint: `sudo ghr-stats …` fails on a user-wide
        // install because sudo's secure_path excludes ~/.cargo/bin, so the hint
        // must name the binary by absolute path.
        let hint = sudo_hint("uninstall");
        assert!(hint.starts_with("sudo /"), "not absolute: {hint}");
        assert!(hint.ends_with(" uninstall"));
    }

    #[test]
    fn sudo_hint_has_no_trailing_space_for_the_bare_binary() {
        assert_eq!(sudo_hint(""), format!("sudo {}", exe_path()));
    }
}
