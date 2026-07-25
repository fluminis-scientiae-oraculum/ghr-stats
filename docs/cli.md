# CLI & operations

[← README](../README.md)

## Commands

```bash
ghr-stats                       # the dashboard (default; `tui` is a hidden alias)
ghr-stats status                # one-shot fleet state; exit code is the verdict
ghr-stats status --json         # the same, machine-readable
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

## status — for scripts and agents

`ghr-stats status` answers "is the fleet healthy right now" in one call, and
encodes the answer in its **exit code** so a caller can branch without parsing:

| Code | Meaning |
| --- | --- |
| 0 | Every runner healthy, and GitHub agrees |
| 1 | Degraded — a runner is offline, divergent, or its GitHub reading is stale |
| 2 | Cannot determine — no collector and no readable runner root |
| 3 | Usage or config error |

```bash
ghr-stats status --json | jq .            # full payload
ghr-stats status --org my-org             # narrow to one org
ghr-stats status --runner runner-01       # narrow to one runner
ghr-stats status --json >/dev/null || echo "fleet is not healthy"
```

Narrowing recomputes the verdict over the rows that remain, so a healthy org is
not reported as degraded because a *different* org is.

The `--json` payload carries `schema_version`, both `generated_at` (ISO-8601 UTC)
and `generated_at_epoch`, per-runner state including `divergent` and
`github_offline_seconds`, and a per-org rollup. It is plain stdout — no colour,
no localised time — and stays clean even under `RUST_LOG=debug`.

When the collector is running, the verdict comes from it (the same snapshot
`/metrics` renders, so the two cannot disagree). With no collector, `status`
falls back to a live local scan, reports `"mode": "ephemeral"`, and leaves every
`github_*` field `null` rather than guessing.

> The SQLite schema is **not** a public interface — it changes without notice.
> Use this verb (or `/metrics`), not ad-hoc SQL.

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
