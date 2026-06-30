# ghr-stats

A mouse-driven terminal dashboard **and** Prometheus exporter for a
**self-hosted GitHub Actions runner fleet**. A `serve` daemon samples every
runner into a local SQLite history; the TUI reads that history to show a live
"now" view, per-runner detail, recent jobs, and time-series trends.

It makes **zero host assumptions** — every fact comes from each runner's own
`.runner` file and its processes/cgroup, so it works on any host running the
standard self-hosted runner, not just the box it was first built for.

```
 Summary  │  Jobs  │  Trends  │  Config  │  Quit
┌ ghr-stats ───────────────────────────────────────────────────────────────┐
│ 20 runners    ● 0 busy    ○ 20 idle    × 0 offline                        │
│ load 1.63    mem 250.7/376.5 GiB (67%)    /tmp 63.9 GiB    free 43.1 TiB  │
│ github: 24 known · 24 online · 0 busy                                     │
└───────────────────────────────────────────────────────────────────────────┘
┌ runners (20) ─────────────────────────────────────────────────────────────┐
│ Runner             Org            Local    GH      CPU    Mem        Up    │
│▌runner-01   example-org   ○ idle   ○ idle  0.0%   171.2 MiB  2d6h  │
│ ...                                                                        │
└───────────────────────────────────────────────────────────────────────────┘
```

## What it shows

- **Summary** — every runner with local liveness, GitHub's view, CPU%, memory,
  uptime; a host header (load / mem / disk); fleet + GitHub counts. A banner
  appears if no fresh samples exist ("start `ghr-stats serve`").
- **Detail** (`Enter` on a runner) — identity (`agentId`/user/dir/group),
  idle/active-since, hook status, the in-flight job, and CPU/mem **charts** with
  a labeled time axis.
- **Jobs** — recent jobs from the runner hooks (repo · workflow · timing).
- **Trends** — fleet occupancy, host load, memory, `/tmp`, and aggregate
  `_work`, each a line chart with a relative-time X axis and a value Y axis.

Keys: `↑↓`/`jk` move · `Enter` detail · `Tab`/`1`–`4` switch tab · `r` refresh ·
`q` quit. Mouse: click a tab or row, scroll the list. From Detail: `R` restart ·
`C` recycle (see [Actions](#actions)).

## Install

**Native (glibc), via cargo** — builds for your CPU, no special toolchain:

```bash
cargo install --path .          # or: cargo install --git <repo-url>
```

**Static binary (musl), for distribution** — one self-contained file that drops
onto any x86-64 Linux host:

```bash
scripts/release.sh              # → target/x86_64-unknown-linux-musl/release/ghr-stats
```

The release build links statically (musl + rustls/ring + bundled SQLite +
mimalloc) and sets `target-cpu=x86-64-v2` via `RUSTFLAGS`. That flag is **not**
pinned in `Cargo.toml` on purpose — pinning it would break `cargo install` for
anyone on a different CPU. Needs a musl C compiler (`musl-gcc`, e.g. Arch
`musl`, Debian `musl-tools`).

## Quick start

```bash
ghr-stats config                       # interactive: discover runners, add PAT(s), set up hooks
ghr-stats systemd install --user       # run `serve` as a per-user service
ghr-stats                              # launch the dashboard (default command)
```

Run `serve` as a **user** service (it runs as the operator that owns the runner
dirs — no root needed) and keep it alive without a login session:

```bash
sudo loginctl enable-linger "$USER"
journalctl --user -u ghr-stats -f
```

For a system-wide deployment (root `serve`, `sudo ghr-stats` TUI), use
`sudo ghr-stats systemd install --system`; it copies the binary to
`/usr/local/bin` so both resolve the same path.

## Configure

`ghr-stats config` is a consent-first wizard: it discovers runner install dirs,
collects an optional GitHub token per org (masked input, validated), and offers
to set up the job hooks. It writes `config.toml` at mode `0600`.

Config search order: `--config FLAG` → `$GHR_STATS_CONFIG` →
`/etc/ghr-stats/config.toml` → `$XDG_CONFIG_HOME/ghr-stats/config.toml`. Every
field has a default, so the tool runs with no config at all. See
[`config.example.toml`](config.example.toml) for every field.

### GitHub API (optional, read-only)

The online/busy reconcile needs a **fine-grained, read-only PAT per org** (a
fine-grained PAT is scoped to one resource owner):

1. GitHub → Settings → Developer settings → Personal access tokens →
   Fine-grained tokens → Generate.
2. Resource owner: the **organization**. Repository access: none.
3. Organization permissions → **Self-hosted runners → Read-only** (only this).
4. Bounded expiration; approve if the org requires it.

The wizard requires the `github_pat_` prefix and rejects classic tokens with a
pointer to the right token type. Tokens are stored under `[github.tokens]`
(org → token) or via `GHR_STATS_GITHUB_TOKEN`, and are never logged.

## Runner hooks

The Jobs/Detail job data comes from the runner's
`ACTIONS_RUNNER_HOOK_JOB_STARTED`/`_COMPLETED` hooks, which append one NDJSON
line per job to a shared event log that `serve` tails. A runner allows **exactly
one** script per hook variable, and many operators already use them — so
ghr-stats **never clobbers**. Per runner it:

- **unset** → installs its script and sets the variable;
- **foreign hook** → offers, per runner, to **chain** (wrap the existing script,
  preserving its exit code, then append our line) or **instruct** (print the
  exact snippet to add yourself);
- **ours** → idempotent no-op.

Scripts always `exit 0` (a non-zero `JOB_STARTED` fails the job). The shared log
must be writable by every runner user and readable by `serve` — see
[`packaging/hooks/README.md`](packaging/hooks/README.md).

## Metrics

`serve` can also expose the fleet metrics (both off by default; enable in
`[metrics]`):

- **Pull** — a tiny `/metrics` endpoint in Prometheus text format, bound to
  **`127.0.0.1:9477`** by default. The metrics are unauthenticated, so the bind
  address must stay on loopback. Always the literal `127.0.0.1`, never
  `localhost`.
- **Push** — periodically POSTs the metrics as JSON to an ingest endpoint (e.g.
  OpenObserve's `_json` API), with an optional `auth` header and an interval.

## Commands

```bash
ghr-stats                       # the dashboard (default; `tui` is a hidden alias)
ghr-stats serve                 # sample the fleet into SQLite + expose metrics
ghr-stats config                # the configuration wizard (orgs / PATs / hooks)
ghr-stats systemd install --user | --system
ghr-stats systemd uninstall
ghr-stats db prune --days 14    # drop time-series samples older than N days
```

Opening the store migrates it, so there is no `db init`. `db prune` keeps
`job_event` and is safe while `serve` writes (SQLite WAL); `VACUUM` separately to
reclaim file space after a large prune.

## Actions

From a runner's Detail view, two remediations run behind a confirm prompt
(direct as root, else via `sudo` on your terminal):

- **Restart** (`R`) — `systemctl restart` the runner's own service to reclaim
  the .NET runner agent's GC memory.
- **Recycle** (`C`, idle-only) — restart **plus** purge that runner's *own*
  `_work/_temp` and trim its `_diag`, scoped strictly to its install dir. It
  **never** touches global `/tmp` or Docker.

## Security

- Config is written `0600`; tokens are redacted in logs and previews.
- The GitHub PAT is fine-grained, read-only, one org per token.
- The metrics pull endpoint binds loopback only.
- Privileged actions are explicit, confirmed, and scoped per runner.

## Platform

Linux only, today. Runner liveness/CPU/memory come from procfs + cgroup v2, the
host sampler reads `/sys` + `statvfs`, and `serve`/`systemd` manage systemd
units — so the build fails fast on other platforms rather than shipping
something that can't sample. A thinner macOS build (launchd + Mac process
introspection, with the TUI as a pure DB reader) is future work.

## Design

- **Fully synchronous, no async runtime.** `serve` is a handful of producer
  threads (local sampler, GitHub reconcile, hook-log tail) feeding a single
  SQLite-writer thread over a `crossbeam-channel`. SQLite runs in WAL so the TUI
  reads while `serve` writes.
- **Single writer, pure reader.** `serve` is the sole sampler and DB writer; the
  TUI only reads.
- **Identity from `.runner`, never from systemd unit names** — `agentId` is the
  exact join key to the GitHub API.
- **Two truths per runner** — local processes and the GitHub API, shown side by
  side so disagreement is visible.

## License

MIT — see [LICENSE](LICENSE).
