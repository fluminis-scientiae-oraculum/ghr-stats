//! Privileged host operations — the sudo model (plan §4/S8/S9).
//!
//! The tool runs as the operator, or as root via `sudo ghr-stats` for a system
//! deployment. Privileged ops run the command directly when already root, else
//! via `sudo` — which prompts on `/dev/tty`, so these must be called only while
//! the TUI is *suspended* (the typestate guarantees this: an action's `execute`
//! runs inside the suspend window). Output is captured for a short decoded
//! result; there is no privilege "dance", just run-the-thing-and-report.

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

/// Whether we are already running as root.
pub(crate) fn is_root() -> bool {
    uzers::get_effective_uid() == 0
}

/// Run a privileged command, capturing its output. Prefixes `sudo` unless we are
/// already root. Call only while the TUI is suspended (so `sudo` can prompt on
/// the real TTY). `argv[0]` is the program.
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
}
