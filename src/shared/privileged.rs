//! Privileged host operations, in two tiers:
//!
//! 1. **Per-command escalation**: [`run`] executes a [`PrivilegedCall`] directly
//!    when root, else via `sudo`. `sudo` prompts on `/dev/tty`, so the TUI calls
//!    it only while suspended.
//! 2. **A root process**: [`require_root`] for flows that write across scopes
//!    (`/etc`, `/usr/local/bin`, runner `.env` files) over several steps.
//!
//! [`PrivilegedCall`] is the closed registry of everything this binary runs
//! elevated; `docs/privileged.md` is its operator-facing summary.

use std::fmt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::shared::collectors::runners::RunnerUnit;
use crate::shared::runner_files::Ownership;

/// The account a command drops to: a runner's own files are removed as that
/// runner, so a planted symlink can only reach what the runner could delete anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunAs {
    pub uid: u32,
    pub gid: u32,
}

/// `systemctl` verbs this tool may invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitVerb {
    Start,
    Stop,
    Restart,
}

impl UnitVerb {
    fn as_str(self) -> &'static str {
        match self {
            UnitVerb::Start => "start",
            UnitVerb::Stop => "stop",
            UnitVerb::Restart => "restart",
        }
    }
}

/// Every command ghr-stats can run with elevated privilege, and the only thing
/// [`run`] accepts. [`fmt::Display`] renders the exact argv, so a confirm prompt
/// and the command that runs are the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrivilegedCall {
    /// `systemctl <verb> <unit>`.
    Systemctl { verb: UnitVerb, unit: RunnerUnit },
    /// `rm -rf -- <dir>`, as the runner.
    PurgeDir { dir: PathBuf, owner: RunAs },
    /// `find <dir> -type f -delete`, as the runner: empties a dir, keeps the dir.
    TrimFilesIn { dir: PathBuf, owner: RunAs },
    /// `install -o <uid> -g <gid> -m <mode> <src> <dst>`: replace a runner's
    /// `.env`, keeping the ownership and mode it had.
    InstallEnvFile {
        src: PathBuf,
        dst: PathBuf,
        ownership: Ownership,
    },
}

impl PrivilegedCall {
    /// The exact `(program, args)` this call executes, passed to `execve` as a
    /// vector, never through a shell.
    fn argv(&self) -> (&'static str, Vec<String>) {
        let path = |p: &PathBuf| p.to_string_lossy().into_owned();
        match self {
            PrivilegedCall::Systemctl { verb, unit } => (
                "systemctl",
                vec![verb.as_str().to_string(), unit.as_str().to_string()],
            ),
            PrivilegedCall::PurgeDir { dir, .. } => {
                ("rm", vec!["-rf".to_string(), "--".to_string(), path(dir)])
            }
            PrivilegedCall::TrimFilesIn { dir, .. } => (
                "find",
                vec![
                    path(dir),
                    "-type".to_string(),
                    "f".to_string(),
                    "-delete".to_string(),
                ],
            ),
            PrivilegedCall::InstallEnvFile {
                src,
                dst,
                ownership,
            } => (
                "install",
                vec![
                    "-o".to_string(),
                    ownership.uid.to_string(),
                    "-g".to_string(),
                    ownership.gid.to_string(),
                    "-m".to_string(),
                    format!("{:04o}", ownership.mode),
                    path(src),
                    path(dst),
                ],
            ),
        }
    }

    fn runs_as(&self) -> Option<RunAs> {
        match self {
            PrivilegedCall::PurgeDir { owner, .. } | PrivilegedCall::TrimFilesIn { owner, .. } => {
                Some(*owner)
            }
            PrivilegedCall::Systemctl { .. } | PrivilegedCall::InstallEnvFile { .. } => None,
        }
    }
}

impl fmt::Display for PrivilegedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (program, args) = self.argv();
        write!(f, "{program}")?;
        for a in &args {
            write!(f, " {a}")?;
        }
        if let Some(who) = self.runs_as() {
            write!(f, " (as uid {})", who.uid)?;
        }
        Ok(())
    }
}

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

/// Run a registered privileged command: directly if root, else via `sudo`.
pub(crate) fn run(call: &PrivilegedCall) -> Outcome {
    let (program, args) = call.argv();
    let mut cmd = match (is_root(), call.runs_as()) {
        (true, None) => Command::new(program),
        (true, Some(who)) => {
            let mut c = Command::new(program);
            c.uid(who.uid).gid(who.gid);
            c
        }
        (false, who) => {
            let mut c = Command::new("sudo");
            if let Some(who) = who {
                c.args(["-u", &format!("#{}", who.uid)]);
            }
            c.arg("--").arg(program);
            c
        }
    };
    match cmd.args(&args).output() {
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
        "Installing runner hooks rewrites each runner's .env and writes shared \
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
    fn every_call_renders_its_exact_argv() {
        let runner = RunAs {
            uid: 1001,
            gid: 1001,
        };
        let cases = [
            (
                PrivilegedCall::Systemctl {
                    verb: UnitVerb::Restart,
                    unit: RunnerUnit::for_test("actions.runner.o.r1.service"),
                },
                "systemctl restart actions.runner.o.r1.service",
            ),
            (
                PrivilegedCall::PurgeDir {
                    dir: PathBuf::from("/srv/runners/r0/_work/_temp"),
                    owner: runner,
                },
                "rm -rf -- /srv/runners/r0/_work/_temp (as uid 1001)",
            ),
            (
                PrivilegedCall::TrimFilesIn {
                    dir: PathBuf::from("/srv/runners/r0/_diag"),
                    owner: runner,
                },
                "find /srv/runners/r0/_diag -type f -delete (as uid 1001)",
            ),
            (
                PrivilegedCall::InstallEnvFile {
                    src: PathBuf::from("/tmp/stage"),
                    dst: PathBuf::from("/srv/runners/r0/.env"),
                    ownership: Ownership {
                        uid: 1001,
                        gid: 1002,
                        mode: 0o600,
                    },
                },
                "install -o 1001 -g 1002 -m 0600 /tmp/stage /srv/runners/r0/.env",
            ),
        ];
        for (call, want) in cases {
            assert_eq!(call.to_string(), want);
        }
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
