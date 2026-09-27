# Privileged operations

[← Docs](README.md)

Sampling, the database, metrics and the dashboard need no privilege. What does is
listed here in full.

| Tier | What it is | Used for |
| --- | --- | --- |
| **Per-command** | one command from a closed list, run directly as root or through `sudo` | restarting and recycling a runner, rewriting a runner's `.env` |
| **Root process** | the whole process must be root, proven by a `Root` value | the entry points [below](#root-process) |
| **Admin peer** | a local user who is root or in the `ghr-stats` group, checked by the collector | PAT and metrics edits over the socket ([Configuration](configuration.md#editing)) |

## Per-command

Every command elevated per call is a variant of one enum, `PrivilegedCall`
([`src/shared/privileged.rs`](../src/shared/privileged.rs)), and the executor
accepts nothing else:

| Variant | Command | Runs as | Used by |
| --- | --- | --- | --- |
| `Systemctl` | `systemctl {start,stop,restart} <unit>` | root | Restart, Recycle, hook install and uninstall |
| `PurgeTemp` | `rm -rf -- <install>/<workFolder>/_temp` | the runner's user | Recycle |
| `TrimDiag` | `find <install>/_diag -type f -delete` | the runner's user | Recycle |
| `InstallEnvFile` | `install -o <uid> -g <gid> -m <mode> <src> <dst>` | root | writing and reverting a runner's `.env` |

- `<unit>` is only ever an `actions.runner.*.service` whose `WorkingDirectory`
  is that runner's install dir.
- `PurgeTemp` and `TrimDiag` take a runner's scratch, built only from that
  runner's own `.runner` and the owner of its install dir, so they name no other
  path or user.
- `InstallEnvFile` keeps the owner and mode the `.env` already had, and runs only
  if the file still matches what was read.
- Deletions drop to the runner's uid, so a planted symlink reaches nothing the
  runner could not already delete.
- Arguments go to `execve` as a vector, never through a shell.
- The Restart prompt renders the same value the executor runs, so the command
  shown is the command run.

Run directly when the process is root, otherwise prefixed with `sudo`, which
prompts on the terminal; the dashboard suspends itself for that.

## Root process

Some work is only correct if this process is root: it writes across scopes, or
derives its install scope from the effective uid, which a per-command `sudo`
cannot change. Those code paths take a `Root` value that only `require_root()`
creates, so they cannot be reached without the check.

| Entry point | Why |
| --- | --- |
| `systemd install --system` | writes `/usr/local/bin` and the system unit; runs `groupadd`, `usermod` and `systemctl` directly |
| hook install (`config`, TUI `[h]`) | shared scripts in `/var/lib/ghr-stats/hooks` that every runner user can read; each runner's `.env` |
| `uninstall hooks`, in either scope | each runner's `.env`, the shared scripts |
| `uninstall` at system scope | removes `/etc`, `/var/lib`, `/usr/local/bin` and the unit |

Without root, each prints the exact command to re-run instead of acting.

## Runner-owned files

A runner's install dir belongs to the runner's user, and its CI jobs run as that
user, so anything in it may be hostile. Opening `.runner`, `.service`, `.env` or
the job-event log refuses symlinks, FIFOs and other non-regular files, hard
links, and files owned by anyone but the runner or root. The first three are read
up to a size cap; the log is read at most 1 MiB per tick. After a `.env` change a
runner is restarted only if it is idle when checked just before the restart.

## `sudo` and `PATH`

`sudo ghr-stats …` often reports "command not found" after `cargo install`:
`sudo` resets `PATH` to a `secure_path` without `~/.cargo/bin`. Every `sudo`
command ghr-stats prints names the binary by absolute path. A system install
copies the binary to `/usr/local/bin`, which is on `secure_path`:

```bash
sudo ~/.cargo/bin/ghr-stats systemd install --system
```

## See also

- [Runner hooks](hooks.md): what the `.env` edits are for
- [CLI & operations](cli.md#per-runner-actions): Restart and Recycle
- [Design](design.md#ipc): how the collector identifies an admin peer
