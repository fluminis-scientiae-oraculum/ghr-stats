# CLI & operations

[← README](../README.md)

## Commands

```bash
ghr-stats                       # the dashboard (default; `tui` is a hidden alias)
ghr-stats serve                 # the collector — systemd-managed; refuses to run on a terminal
ghr-stats config                # the configuration wizard (orgs / PATs / hooks)
ghr-stats systemd install --user | --system   # install/enable the collector service
ghr-stats systemd uninstall
ghr-stats db prune --days 14    # drop time-series samples older than N days
ghr-stats uninstall             # dry-run plan of everything installed (removes nothing)
ghr-stats uninstall all --yes   # remove it all — hooks, service, config, data, binary
```

You don't run `serve` yourself — `systemd install` does. Opening the store
migrates it, so there is no `db init`. `db prune` keeps `job_event` and is safe
while the collector writes (SQLite WAL); `VACUUM` separately to reclaim file
space after a large prune.

## Uninstall

`ghr-stats uninstall` is the inverse of install, and just as careful. With no
argument it prints a **dry-run plan** of everything on the host and removes
nothing — a safe "what's installed" view. Name one or more domains (or `all`) to
actually remove:

```bash
ghr-stats uninstall                 # dry-run plan, removes nothing
ghr-stats uninstall hooks           # just revert the runner hooks
ghr-stats uninstall config data     # remove the PAT config + SQLite history
sudo ghr-stats uninstall all --yes  # everything, no prompt
```

Domains: `hooks` · `service` · `config` · `data` · `binary` · `all`. You are
asked to confirm before anything is removed unless `--yes` is given.

Hooks are reverted **detect-first, never stranding a foreign hook**: a runner
ghr-stats *chained* is restored to its original hook (recovered from the wrapper),
a runner it installed *fresh* goes back to unset, and a foreign or untouched
runner is left exactly as-is. Editing runner `.env` files needs root, same as
install; a busy runner keeps running and picks up the reverted `.env` on its next
restart.

Removing `config` deletes the file holding your PAT(s). It is **unlinked, not
shredded** — on modern copy-on-write / SSD filesystems an overwrite doesn't reach
the underlying blocks, so ghr-stats doesn't pretend to. To be sure a token is
dead, **revoke it on GitHub**. The installed binary copy is removed; a `cargo
install` build prints `cargo uninstall fso-ghr-stats` instead (Cargo owns
`~/.cargo/bin`). Nothing sensitive is ever printed — the plan shows a token
**count**, never a value.

## Per-runner actions (Detail view)

From a runner's Detail view, two remediations run behind a confirm prompt
(direct as root, else via `sudo` on your terminal):

- **Restart** (`R`) — `systemctl restart` the runner's own service to reclaim
  the .NET runner agent's GC memory.
- **Recycle** (`C`, idle-only) — restart **plus** purge that runner's *own*
  `_work/_temp` and trim its `_diag`, scoped strictly to its install dir. It
  **never** touches global `/tmp` or Docker.
