//! Privileged host operations. Two distinct needs, deliberately not unified:
//!
//! 1. **Per-command escalation** — [`run`] executes a [`PrivilegedCall`]
//!    directly when already root, else via `sudo`. Enough whenever each command
//!    can escalate on its own (`systemctl restart`, `install(1)`). `sudo` prompts
//!    on `/dev/tty`, so call only while the TUI is *suspended* — the typestate
//!    guarantees an action's `execute` runs inside the suspend window.
//! 2. **A root *process*** — [`require_root`] / [`is_root`], for work whose
//!    correctness depends on the process itself being root: it writes across
//!    scopes (`/etc`, `/usr/local/bin`, root-owned runner `.env` files) or must
//!    resolve our own install scope. Per-op `sudo` cannot supply this. Gated at
//!    the three entry points that need it — `ops::systemd::install`,
//!    `ops::wizard::apply_hooks`, `ops::uninstall`.
//!
//! Tier 1 is a *registry*: [`PrivilegedCall`] is a closed enum of every command
//! this binary can run elevated, and [`run`] accepts nothing else. The privilege
//! surface is therefore readable in one place instead of reconstructed by
//! grepping call sites, and widening it means adding a variant — a deliberate
//! edit that shows up in review. Operator-facing summary: `docs/privileged.md`.
//!
//! Tier 2 stays free functions. A `PrivilegedExecution` template-method trait
//! lived here until 0.2.1 and was removed: it wrapped only the two TUI actions,
//! which need tier 1 and never overrode the gate, while all four sites needing
//! tier 2 called the free functions directly — so it advertised an enforcement it
//! did not provide. See the backlog entry for D1.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

/// The `.env` mode is fixed here, not passed in: the wizard *writes* these files
/// and `uninstall` *reverts* them, and the two directions must stay symmetric on
/// ownership and mode. A parameter would let them drift.
const ENV_MODE: &str = "0644";

/// `systemctl` verbs this tool may invoke. Closed on purpose — see
/// [`PrivilegedCall`].
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

/// Every command ghr-stats can run with elevated privilege — the complete
/// registry, and the only thing [`run`] accepts.
///
/// [`fmt::Display`] renders the exact argv, so a confirm prompt and the command
/// that actually runs cannot disagree: they are the same value, formatted once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrivilegedCall {
    /// `systemctl <verb> <unit>` — bounce a runner's service.
    Systemctl { verb: UnitVerb, unit: String },
    /// `rm -rf -- <dir>`. The caller scopes `dir` to a runner's own install dir
    /// (see `RecycleRunner::scoped_paths`); never a global path.
    PurgeDir { dir: PathBuf },
    /// `find <dir> -type f -delete` — empty a dir, keeping the dir itself.
    TrimFilesIn { dir: PathBuf },
    /// `install -o <owner> -g <owner> -m 0644 <src> <dst>` — stage a runner's
    /// root-owned `.env` preserving ownership and mode.
    InstallEnvFile {
        owner: String,
        src: PathBuf,
        dst: PathBuf,
    },
}

impl PrivilegedCall {
    /// The exact `(program, args)` this call executes — the ONLY place a
    /// privileged argv is built. Arguments are passed to `execve` as a vector,
    /// never through a shell, so no quoting or escaping applies.
    fn argv(&self) -> (&'static str, Vec<String>) {
        let path = |p: &PathBuf| p.to_string_lossy().into_owned();
        match self {
            PrivilegedCall::Systemctl { verb, unit } => {
                ("systemctl", vec![verb.as_str().to_string(), unit.clone()])
            }
            PrivilegedCall::PurgeDir { dir } => {
                ("rm", vec!["-rf".to_string(), "--".to_string(), path(dir)])
            }
            PrivilegedCall::TrimFilesIn { dir } => (
                "find",
                vec![
                    path(dir),
                    "-type".to_string(),
                    "f".to_string(),
                    "-delete".to_string(),
                ],
            ),
            PrivilegedCall::InstallEnvFile { owner, src, dst } => (
                "install",
                vec![
                    "-o".to_string(),
                    owner.clone(),
                    "-g".to_string(),
                    owner.clone(),
                    "-m".to_string(),
                    ENV_MODE.to_string(),
                    path(src),
                    path(dst),
                ],
            ),
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

/// Run a registered privileged command — directly if root, else via `sudo`.
///
/// Taking a [`PrivilegedCall`] rather than a `(program, args)` pair is what
/// makes the registry binding: there is no way to run an unregistered command,
/// and no "empty command" to guard against at runtime.
pub(crate) fn run(call: &PrivilegedCall) -> Outcome {
    let (program, args) = call.argv();
    let mut cmd = if is_root() {
        Command::new(program)
    } else {
        let mut c = Command::new("sudo");
        c.arg(program);
        c
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

    /// Display IS the argv — this is what makes a confirm prompt unable to
    /// misreport what will run, so pin the rendering of every variant.
    #[test]
    fn every_call_renders_its_exact_argv() {
        assert_eq!(
            PrivilegedCall::Systemctl {
                verb: UnitVerb::Restart,
                unit: "runner-1.service".into(),
            }
            .to_string(),
            "systemctl restart runner-1.service"
        );
        assert_eq!(
            PrivilegedCall::PurgeDir {
                dir: PathBuf::from("/srv/runners/r0/_work/_temp"),
            }
            .to_string(),
            "rm -rf -- /srv/runners/r0/_work/_temp"
        );
        assert_eq!(
            PrivilegedCall::TrimFilesIn {
                dir: PathBuf::from("/srv/runners/r0/_diag"),
            }
            .to_string(),
            "find /srv/runners/r0/_diag -type f -delete"
        );
        assert_eq!(
            PrivilegedCall::InstallEnvFile {
                owner: "runner".into(),
                src: PathBuf::from("/tmp/stage"),
                dst: PathBuf::from("/srv/runners/r0/.env"),
            }
            .to_string(),
            "install -o runner -g runner -m 0644 /tmp/stage /srv/runners/r0/.env"
        );
    }

    #[test]
    fn unit_verbs_map_to_systemctl_subcommands() {
        for (verb, want) in [
            (UnitVerb::Start, "start"),
            (UnitVerb::Stop, "stop"),
            (UnitVerb::Restart, "restart"),
        ] {
            let call = PrivilegedCall::Systemctl {
                verb,
                unit: "u.service".into(),
            };
            assert_eq!(call.argv().1[0], want);
        }
    }

    /// `rm -rf` must keep `--` immediately before the path: without it a dir
    /// whose name begins with `-` would be parsed as a flag.
    #[test]
    fn purge_terminates_options_before_the_path() {
        let (program, args) = PrivilegedCall::PurgeDir {
            dir: PathBuf::from("/srv/r/_temp"),
        }
        .argv();
        assert_eq!(program, "rm");
        assert_eq!(args[args.len() - 2], "--");
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
