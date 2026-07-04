# ghr-stats

[![crates.io](https://img.shields.io/crates/v/ghr-stats.svg?logo=rust)](https://crates.io/crates/ghr-stats)
[![docs.rs](https://img.shields.io/docsrs/ghr-stats?logo=docsdotrs)](https://docs.rs/ghr-stats)
[![CI](https://github.com/fluminis-scientiae-oraculum/ghr-stats/actions/workflows/on-pr-sync.yml/badge.svg)](https://github.com/fluminis-scientiae-oraculum/ghr-stats/actions/workflows/on-pr-sync.yml)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue?logo=rust)](https://releases.rs)
[![License: MIT](https://img.shields.io/crates/l/ghr-stats.svg)](LICENSE)

> **Live TUI + Prometheus exporter for a self-hosted GitHub Actions runner fleet.**
> Zero host assumptions, zero-setup standalone mode, and an optional collector
> service for history, a jobs timeline, and metrics when you want them.

A mouse-driven terminal dashboard **and** Prometheus exporter for a
**self-hosted GitHub Actions runner fleet**. The TUI runs in two modes: an
**Ephemeral** live dashboard standalone (no service, no database), or a
**Persistent** dashboard once the collector service is installed — adding SQLite
history, recent jobs, the GitHub reconcile, and a Prometheus exporter.

It makes **zero host assumptions** — every fact comes from each runner's own
`.runner` file and its processes/cgroup, so it works on any host running the
standard self-hosted runner, not just the box it was first built for.

```text
 Summary  │  Jobs  │  Trends  │  Config  │  Quit
┌ ghr-stats ───────────────────────────────────────────────────────────────┐
│ 8 runners    ● 1 busy    ○ 7 idle    × 0 offline                          │
│ load 0.42    mem 9.7/31.3 GiB (31%)    /tmp 2.1 GiB    free 612.4 GiB     │
│ github: 8 known · 8 online · 1 busy                                       │
└───────────────────────────────────────────────────────────────────────────┘
┌ runners (8) ──────────────────────────────────────────────────────────────┐
│ Runner        Org           Local    For    Hook   GH      CPU    Mem      │
│▌runner-01     example-org   ● busy   4m2s   ✓      ● busy  38.4%  1.2 GiB  │
│ runner-02     example-org   ○ idle   1h3m   ✓      ○ idle  0.0%   172 MiB  │
│ ...                                                                        │
└───────────────────────────────────────────────────────────────────────────┘
```

## Highlights

- **Zero setup** — the Ephemeral TUI works the instant you run it; no service, no
  database, no root.
- **Two truths, side by side** — local process/cgroup liveness *and* GitHub's API
  view, so any disagreement is visible at a glance.
- **Optional collector** — a systemd service adds SQLite history, a Jobs timeline
  from the runner hooks, and a Prometheus exporter (pull *and* push).
- **Zero host assumptions** — every fact from each runner's own `.runner` file +
  procfs/cgroup v2; no systemd-unit-name parsing, no per-host config.
- **Fully synchronous** — no async runtime; a few producer threads feed one
  SQLite writer over a channel.
- **Single static binary** — a musl `x86-64-v2` build drops onto any Linux host;
  tokens are fine-grained, read-only, and never logged.

## What it shows

- **Summary** — every runner with local liveness, GitHub's view, CPU%, memory,
  uptime; a host header (load / mem / disk); fleet + GitHub counts. A mode badge
  (top-right) reads **EPHEMERAL** or **PERSISTENT**; the GitHub counts appear
  only in Persistent mode.
- **Detail** (`Enter` on a runner) — identity (`agentId`/user/dir/group),
  idle/active-since, hook status, the in-flight job, and CPU/mem **charts** with
  a labeled time axis.
- **Jobs** — recent jobs from the runner hooks (repo · workflow · timing).
- **Trends** — fleet occupancy, host load, memory, `/tmp`, and aggregate
  `_work`, each a line chart with a relative-time X axis and a value Y axis.

Keys: `↑↓`/`jk` move · `Enter` detail · `Tab`/`1`–`4` switch tab · `r` refresh ·
`?` help · `q` quit. From Detail: `Esc` back · `R` restart · `C` recycle. From
Config: `a` add org+PAT · `h` install hooks · `m` toggle metrics · `o` open
config. Mouse: click a tab or row, scroll the list. (See [Actions](#actions).)
The footer shows the keys that apply to the current view; `?` opens full help.

## Modes

The dashboard adapts to whether the collector service is running:

- **Ephemeral** (no service) — the TUI samples the fleet itself, in memory, every
  couple of seconds. You get the live Summary, per-runner detail, and short
  rolling Trends/sparklines covering *since you launched it*. Nothing is written
  to disk; there is no GitHub reconcile and no Jobs history. Zero setup.
- **Persistent** (collector installed) — `ghr-stats systemd install` runs the
  collector as a service: it samples into SQLite, reconciles GitHub, tails the
  job hooks, and exposes Prometheus metrics. The TUI detects the collector over a
  Unix socket and pulls full history, Jobs, and the GitHub view from it.

The TUI never opens the database directly — it talks to the collector over a
loopback Unix socket. That keeps the dashboard a zero-privilege client and lets a
**non-root TUI observe a root system service**: the socket crosses the scope
boundary, while the `/var/lib` database (which only the service may open, WAL
needing writer access to the directory) does not. The collector (`serve`) is
managed by systemd and **refuses to run on a terminal** — you never invoke it by
hand (set `GHR_STATS_ALLOW_TTY=1` only for dev/CI).

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
ghr-stats                              # Ephemeral dashboard — works immediately, no setup
ghr-stats config                       # interactive: discover runners, add PAT(s), set up hooks
ghr-stats systemd install --user       # install the collector → Persistent mode
```

Installing the collector as a **user** service runs it as the operator that owns
the runner dirs (no root needed); keep it alive without a login session:

```bash
sudo loginctl enable-linger "$USER"
journalctl --user -u ghr-stats -f
```

For a system-wide deployment, `sudo ghr-stats systemd install --system` runs the
collector as root under `/var/lib`. The TUI reaches it over the socket, so you
can run the dashboard as **either** a plain user or `sudo ghr-stats`. Install
copies the binary to `/usr/local/bin` so the unit and a later `sudo` resolve the
same file.

## Configure

`sudo ghr-stats config` is a consent-first wizard: it discovers runner install
dirs, collects an optional GitHub token per org (masked input, validated), and
offers to set up the job hooks. It writes the config at mode `0600`.

The config is a **single system-scope artifact at `/etc/ghr-stats/config.toml`**
(root:root, `0600`) — it holds your PATs and is the collector's source of truth,
so it lives there once rather than duplicated per-user. Writing it needs root
(`sudo`); reading it does too, so run the dashboard as `sudo ghr-stats` for the
live view (a non-root TUI still shows persistent data over the socket, but has no
local config). An explicit `--config FLAG` or `$GHR_STATS_CONFIG` overrides the
location. Every field has a default, so the tool runs with no config at all. See
[`config.example.toml`](config.example.toml) for every field.

### Editing config from a non-root TUI (the `ghr-stats` group)

Because the config is root-owned, a plain dashboard normally can't apply the
Config-tab edits — you'd run `sudo ghr-stats`. To avoid per-edit sudo, a system
install provisions a **`ghr-stats` group** and adds the installing operator to
it. When the collector is running, the TUI's `[a]` (add org + PAT) and `[m]`
(toggle metrics) route the change over the socket to the root collector, which
applies it only for an **authorized peer** — one whose kernel-reported uid is
root or a member of `ghr-stats`. This is the standard privileged-daemon pattern:
the unprivileged client asks the privileged service to perform a narrow,
validated write on its behalf. Reads stay open to any local user (the socket
carries only derived fleet stats, never tokens); only these two writes are gated,
and the token itself is one-way — written, never returned over the socket.

Membership is resolved fresh by the collector on every request, so
`sudo usermod -aG ghr-stats <user>` takes effect immediately — no re-login. An
unauthorized edit is refused with guidance rather than silently failing. Hooks
(`[h]`) and the raw-file editor (`[o]`) still shell out with sudo.

### GitHub API (optional, read-only)

> **Organization runners only, for the GitHub-side reconcile.** The reconcile
> calls `GET /orgs/{org}/actions/runners`, which is gated by the **organization**
> "Self-hosted runners" fine-grained-PAT permission — a permission that exists
> only on an *organization* resource owner. **Personal-account (repository-level)
> self-hosted runners have no equivalent PAT permission**, so they get no GitHub
> online/busy reconcile and no job pass/fail conclusions. They are still **fully
> sampled locally** — process/cgroup liveness, CPU, memory, uptime — with no PAT
> at all; only GitHub's own view of the runner is unavailable for them.

The online/busy reconcile needs a **fine-grained, read-only PAT per org** (a
fine-grained PAT is scoped to one resource owner):

1. GitHub → Settings → Developer settings → Personal access tokens →
   Fine-grained tokens → Generate.
2. Resource owner: the **organization**.
3. Organization permissions → **Self-hosted runners → Read-only** — **required**
   (runner online/busy).
4. Repository permissions → **Actions → Read-only** — **optional**: lets the
   collector fill each finished job's **success/failure** in the Jobs view.
   Without it, jobs still show timing, just a neutral "done".
5. Bounded expiration; approve if the org requires it.

> **Note on repository access.** The **Actions** permission only appears when
> "Repository access" is **All repositories** or **Only select repositories** —
> the **"Public repositories"** option is a fixed read-only scope that exposes no
> repository permissions (and can't see private repos, where self-hosted runners
> usually live). Pick All/selected repos to get "Actions: Read". The
> **Self-hosted runners** permission is organization-scoped and independent of
> this.

The wizard requires the `github_pat_` prefix and rejects classic tokens with a
pointer to the right token type. Tokens are stored under `[github.tokens]`
(org → token) or via `GHR_STATS_GITHUB_TOKEN`, and are never logged.

## Runner hooks

The Jobs/Detail job data comes from the runner's
`ACTIONS_RUNNER_HOOK_JOB_STARTED`/`_COMPLETED` hooks, which append one NDJSON
line per job to a shared event log that the collector tails. A runner allows
**exactly one** script per hook variable, and many operators already use them — so
ghr-stats **never clobbers**. Per runner it:

- **unset** → installs its script and sets the variable;
- **foreign hook** → offers, per runner, to **chain** (wrap the existing script,
  preserving its exit code, then append our line) or **instruct** (print the
  exact snippet to add yourself);
- **ours** → idempotent no-op.

Scripts always `exit 0` (a non-zero `JOB_STARTED` fails the job). The shared log
must be writable by every runner user and readable by the collector — see
[`packaging/hooks/README.md`](packaging/hooks/README.md).

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
install` build prints `cargo uninstall ghr-stats` instead (Cargo owns
`~/.cargo/bin`). Nothing sensitive is ever printed — the plan shows a token
**count**, never a value.

## Metrics

The collector can also expose the fleet metrics (both off by default; enable in
`[metrics]`). These are Persistent-mode features — they need the service:

- **Pull** — a tiny `/metrics` endpoint in Prometheus text format, bound to
  **`127.0.0.1:9477`** by default. The metrics are unauthenticated, so the bind
  address must stay on loopback. Always the literal `127.0.0.1`, never
  `localhost`.
- **Push** — periodically POSTs the metrics as JSON to an ingest endpoint (e.g.
  OpenObserve's `_json` API), with an optional `auth` header and an interval.

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
host sampler reads `/sys` + `statvfs`, the collector's IPC uses an `AF_UNIX`
socket, and `systemd` manages the service — so the build fails fast on other
platforms rather than shipping something that can't sample. A thinner macOS build
(launchd + Mac process introspection) is future work.

## Design

- **Fully synchronous, no async runtime.** The collector is a handful of producer
  threads (local sampler, GitHub reconcile, hook-log tail) feeding a single
  SQLite-writer thread over a `crossbeam-channel`; metrics and the TUI's IPC read
  on their own WAL connections. The TUI↔collector link is a length-prefixed JSON
  protocol over a Unix socket — no HTTP framework, no runtime.
- **Single writer, DB-agnostic client.** The collector is the sole DB writer and
  reader; the TUI never opens the database — in Ephemeral mode it samples
  in-memory, in Persistent mode it reads through the socket.
- **Identity from `.runner`, never from systemd unit names** — `agentId` is the
  exact join key to the GitHub API.
- **Two truths per runner** — local processes and the GitHub API, shown side by
  side so disagreement is visible.

## License

MIT — see [LICENSE](LICENSE).
