# ghr-stats

[![crates.io](https://img.shields.io/crates/v/fso-ghr-stats.svg?logo=rust)](https://crates.io/crates/fso-ghr-stats)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue?logo=rust)](https://releases.rs)
[![License: MIT](https://img.shields.io/crates/l/fso-ghr-stats.svg)](LICENSE)

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
│ 8 runners    ● 1 busy    ○ 7 idle    × 0 offline                         │
│ load 0.42    mem 9.7/31.3 GiB (31%)    /tmp 2.1 GiB    free 612.4 GiB    │
│ github: 8 known · 8 online · 1 busy                                      │
└──────────────────────────────────────────────────────────────────────────┘
┌ runners (8) ─────────────────────────────────────────────────────────────┐
│ Runner        Org           Local    For    Hook   GH      CPU    Mem    │
│▌runner-01     example-org   ● busy   4m2s   ✓      ● busy  38.4%  1.2 GiB│
│ runner-02     example-org   ○ idle   1h3m   ✓      ○ idle  0.0%   172 MiB│
│ ...                                                                      │
└──────────────────────────────────────────────────────────────────────────┘
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
config. Mouse: click a tab, a runner row, or any footer hint; **double-click a
runner** opens its detail; scroll the list. The footer shows the keys that apply
to the current view; `?` opens full help.

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

**Native (glibc), via cargo** — builds for your CPU, no special toolchain. The
crate is `fso-ghr-stats`; it installs a binary named `ghr-stats`:

```bash
cargo install fso-ghr-stats     # from crates.io
cargo install --path .          # or from a checkout (or: cargo install --git <repo-url>)
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

## Documentation

- **[Configuration](docs/configuration.md)** — the config file, the `ghr-stats`
  group for non-root edits, and read-only GitHub PATs per org. Every field is in
  [`config.example.toml`](config.example.toml).
- **[Runner hooks](docs/hooks.md)** — how Jobs/Detail timing is collected without
  clobbering existing hooks. Details: [`packaging/hooks/README.md`](packaging/hooks/README.md).
- **[Metrics](docs/metrics.md)** — the Prometheus pull endpoint and the JSON push.
- **[CLI & operations](docs/cli.md)** — every command, uninstall, and the
  per-runner Restart/Recycle actions.
- **[Design & internals](docs/design.md)** — architecture, the sync (no-async)
  model, and platform support.

## Security

- Config is written `0600`; tokens are redacted in logs and previews.
- The GitHub PAT is fine-grained, read-only, one org per token.
- The metrics pull endpoint binds loopback only.
- Privileged actions are explicit, confirmed, and scoped per runner.

## Platform

Linux only (procfs + cgroup v2, `/sys`, `AF_UNIX`, systemd) — see
[Design & internals](docs/design.md#platform).

## License

MIT — see [LICENSE](LICENSE).
