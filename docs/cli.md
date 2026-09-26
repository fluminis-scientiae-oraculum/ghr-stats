# CLI & operations

[← Docs](README.md)

| Command | For | Does |
| --- | --- | --- |
| `ghr-stats` | humans | the dashboard (`tui` is a hidden alias) |
| `status`, `explain`, `timeline`, `doctor`, `wait`, `tail` | scripts, agents | see [For agents](agents.md) |
| `config` | operators, root | the setup wizard: runners, PATs, metrics, hooks ([Configuration](configuration.md)) |
| `systemd install --system` \| `--user` | operators | install and start the collector |
| `systemd uninstall` | operators | stop and remove the unit; data stays |
| `db prune [--days N]` | operators | delete samples older than N days, once; without `--days`, per `retention_days` (refused when it is `"forever"`) |
| `uninstall [DOMAIN…]` | operators | reverse an install; no domain = dry-run plan |
| `serve` | systemd | the collector; refuses to run on a terminal |

Every command takes `--config FILE`. `--help` on any command is the full
reference.

## The collector service

```bash
sudo ghr-stats systemd install --system   # root collector; binary copied to /usr/local/bin
ghr-stats systemd install --user          # collector as you; binary copied to ~/.local/bin
```

Install copies the running binary to a fixed path so the unit and a later
`sudo ghr-stats` run the same file, writes and enables the unit, and (re)starts
it. A system install also sets up the `ghr-stats` group for
[non-root PAT edits](configuration.md#editing). A user service needs lingering to
run without a login session:

```bash
sudo loginctl enable-linger "$USER"
journalctl --user -u ghr-stats -f
```

After upgrading, re-run the same `systemd install` from the new binary: it
replaces the fixed-path copy and restarts the service. Until then, clients refuse
to talk to a collector of another IPC version.

Opening the database migrates it; there is no `db init`. The collector prunes on
its own per `retention_days`; `db prune` is for a one-off cut and is safe while
the collector runs. Neither shrinks the file: stop the service and run
`sqlite3 <db_path> VACUUM` to return the space.

## Per-runner actions

In the dashboard, on a runner's Detail view, behind a confirm prompt (run
directly as root, else through `sudo` on your terminal):

| Key | Action |
| --- | --- |
| `R` | **Restart**: `systemctl restart` that runner's own unit, returning the .NET agent's memory. On a busy runner the prompt warns that the job is cancelled. |
| `C` | **Recycle**, idle runners only (checked again just before it runs): stop, empty that runner's `<workFolder>/_temp`, delete its `_diag` files, start. Never global `/tmp` or Docker. |

See [Privileged operations](privileged.md) for exactly what runs.

## Uninstall

```bash
sudo ghr-stats uninstall                  # dry-run plan: what is installed, removes nothing
sudo ghr-stats uninstall hooks            # revert runner hooks only
sudo ghr-stats uninstall config data      # remove the config (PATs) and the database
sudo ghr-stats uninstall all --yes        # everything, no prompt
```

The scope follows the effective uid (`--system` / `--user` force it): without
`sudo`, uninstall acts on the user scope.

| Domain | Removes |
| --- | --- |
| `hooks` | our three variables from each runner's `.env` (chained runners get their original hook back), our scripts and wrappers; needs root in either scope |
| `service` | the systemd unit |
| `config` | the config file |
| `data` | the database, its WAL files, the lock, and a shared event log left by older releases |
| `binary` | the installed copy; for a `cargo install` build, prints `cargo uninstall fso-ghr-stats` instead |
| `all` | all of the above |

Removal applies to one scope: `--user` never touches `/etc` or `/var/lib`, and a
system-scope removal needs root up front. Hooks are reverted only where both
variables point at our scripts or both at our chain wrappers; any other mix is
reported and left alone. The `.env` is rewritten at once; a busy runner keeps its
old environment until its next restart.

The config is **unlinked, not shredded**: overwriting does not reach the blocks
on copy-on-write or SSD storage. To be sure a token is dead, revoke it on
GitHub. The plan shows how many tokens a config holds, never their values.

## See also

- [For agents](agents.md): the machine-facing verbs in full
- [Runner hooks](hooks.md): what `uninstall hooks` reverses
- [Privileged operations](privileged.md): what needs root and why
