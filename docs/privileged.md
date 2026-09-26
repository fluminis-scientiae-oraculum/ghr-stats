# Privileged operations

[← README](../README.md)

ghr-stats collects everything it can without elevation: sampling runners, reading
cgroups, the database and the metrics endpoints all run unprivileged. A handful of
*actions* do need elevation, and this page is the complete account of them.

## What runs as root

Every elevated command is a variant of one closed enum, `PrivilegedCall`
(`src/shared/privileged.rs`). `run()` accepts nothing else, so this table is the
whole privilege surface — not a sample of it:

| Variant | Exact command | Runs as | Used by |
| --- | --- | --- | --- |
| `Systemctl` | `systemctl {start,stop,restart} <unit>` | root | Restart / Recycle, hook install and uninstall |
| `PurgeDir` | `rm -rf -- <dir>` | the runner user | Recycle: `<install>/<work>/_temp` |
| `TrimFilesIn` | `find <dir> -type f -delete` | the runner user | Recycle: `<install>/_diag` |
| `InstallEnvFile` | `install -o <uid> -g <gid> -m <mode> <src> <dst>` | root | Writing and reverting a runner's `.env` |

`<unit>` is only ever a unit named `actions.runner.*.service` whose
`WorkingDirectory` is that runner's install dir. `InstallEnvFile` keeps the
uid, gid and mode the `.env` already had.

Three properties follow from that being an enum rather than a free
`(program, args)` pair:

- **Auditing is reading, not grepping.** Widening what this tool can do as root
  means adding a variant, which is a visible diff in review.
- **The prompt cannot lie.** `Display` on `PrivilegedCall` renders the exact
  argv, and the confirm popup formats the same value the executor runs. There is
  no second copy of the command string to drift.
- **Ownership travels with the file.** The wizard writes `.env` files and
  `uninstall` reverts them; both carry the ownership read from the file itself.

Arguments are handed to `execve` as a vector and never to a shell, so no quoting
or escaping is involved.

## Runner-owned files

A runner's install dir belongs to the runner user, and its CI jobs run as that
user, so anything in it may have been planted. Reads of `.runner`, `.service`,
`.env` and the job-event log refuse symlinks, FIFOs and other non-regular files,
hard links, files owned by anyone but the runner or root, and anything past a
size cap. Deleting a runner's files drops to that runner's uid, so a planted
symlink reaches only what the runner could delete anyway. Runner restarts after
a `.env` change happen only when the runner is idle.

## Two ways to be privileged

They are not interchangeable, and picking the wrong one is how a gate gets
forgotten.

**Per-command escalation** — `run()` executes directly when the process is
already root, otherwise it prepends `sudo`. This suffices when each command can
escalate on its own, which is true of everything in the table above. `sudo`
prompts on `/dev/tty`, so the TUI suspends itself first; an action's `execute`
is guaranteed by the typestate to run inside that suspend window.

**A root process** — `require_root()` / `is_root()`, for work whose correctness
depends on *this process* being root, because it writes across scopes or has to
resolve our own install scope (which is derived from the effective uid and so
cannot be relocated by a per-command `sudo`). Three entry points need it:

| Entry point | Why a root process |
| --- | --- |
| `systemd install --system` | writes `/etc`, `/usr/local/bin`, the system unit |
| hook install (`config`, TUI `[h]`) | shared scripts must be readable by every runner user; each runner's `.env` belongs to its runner user |
| `uninstall` at system scope | removes `/etc`, `/var/lib`, `/usr/local/bin` and the unit — refused up front rather than half-done |

Each refuses with a re-run hint instead of failing partway.

## The `sudo` PATH gotcha

`sudo ghr-stats …` often reports "command not found" after a user-wide install.
That is `sudo` resetting `PATH` to a `secure_path` that excludes `~/.cargo/bin`
and `~/.local/bin` — not a broken install.

Every hint this tool prints therefore names the binary by **absolute path**, so
it works as printed. To put ghr-stats on `sudo`'s path permanently:

```sh
ghr-stats systemd install --system   # copies the binary to /usr/local/bin
```
