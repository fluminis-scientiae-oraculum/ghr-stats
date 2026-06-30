# ghr-stats

A terminal dashboard for a **self-hosted GitHub Actions runner fleet**, built
for the `fso-epoch` host (one NUMA box, ~20 runners across several orgs, one
shared dockerd). It samples the fleet into SQLite continuously and renders both
a live "now" view and historical trends.

```
┌ ghr-stats · fso-epoch ─────────────────────────────────────────┐
│ 20 runners    ● 0 busy    ○ 20 idle    × 0 offline              │
│ load 1.66    mem 231/376 GiB (61%)    /tmp 63.9 GiB   free 43 TiB│
│ github: 24 known · 24 online · 0 busy                           │
└─────────────────────────────────────────────────────────────────┘
┌ runners (20) ───────────────────────────────────────────────────┐
│ Runner            Org              Local   GH      CPU   Mem  Up  │
│▌fso-epoch-fso-00  fluminis-...     ○ idle  ○ idle  0.0%  171M 2d  │
│ ...                                                              │
└─────────────────────────────────────────────────────────────────┘
```

## What it shows

- **Overview** — every runner with local liveness, GitHub's view, CPU%, memory,
  uptime; host load/mem/disk; fleet + GitHub counts.
- **Runner detail** (`Enter`) — identity, live stats, CPU/mem history sparklines.
- **Trends** (`Tab`/`t`) — fleet occupancy, host load, memory, `/tmp`, and
  aggregate `_work` over time.

Keys: `↑↓`/`jk` move · `Enter` detail · `Tab`/`t` trends · `Esc` back · `r`
refresh · `q` quit.

## Design notes

- **Identity comes from each runner's own `.runner` file** (`agentId`,
  `gitHubUrl`→org, `poolName`, `agentName`) — never from parsing systemd unit
  names. `agentId` is the join key to the GitHub API.
- **Liveness/resources are read locally**: a runner's owning-uid processes
  (`Runner.Listener` = online, `Runner.Worker` = busy) and its cgroup
  (`memory.current`, `cpu.stat`). Everything is world-readable, so no privilege
  is needed.
- **Two independent truths per runner**: local (processes) and GitHub (API
  `busy`/`status`). Shown side by side, so disagreement is visible.
- **Fully synchronous**: a `collect` daemon with two producer threads
  (local sampler + GitHub reconcile) feeding a single SQLite-writer over a
  `crossbeam-channel`. No async runtime. SQLite runs in **WAL** mode so the TUI
  reads while the collector writes.

## Build & install

```bash
cargo build --release
install -Dm755 target/release/ghr-stats ~/.local/bin/ghr-stats
```

Run the collector as a systemd **user** service (it runs as the operator that
owns the runner dirs — never as root):

```bash
install -Dm644 packaging/ghr-stats-collector.service \
    ~/.config/systemd/user/ghr-stats-collector.service
systemctl --user daemon-reload
systemctl --user enable --now ghr-stats-collector.service
sudo loginctl enable-linger "$USER"   # keep it running without a login session
journalctl --user -u ghr-stats-collector -f
```

## Configure

Copy `config.example.toml` to `~/.config/ghr-stats/config.toml` (mode `0600` if
it holds tokens). All paths default to user-scoped XDG locations. See the
example file for every field.

### GitHub API (optional, read-only)

The GitHub reconcile + the `github` command need a **fine-grained, read-only**
PAT **per org** (a fine-grained PAT is scoped to a single resource owner):

1. GitHub → Settings → Developer settings → Personal access tokens →
   Fine-grained tokens → Generate.
2. Resource owner: the **organization**. Repository access: none/public.
3. Organization permissions → **Self-hosted runners → Read-only** (only this).
4. Bounded expiration; approve if the org requires it.

Put each under `[github.tokens]` (org → token), or pass one via
`GHR_STATS_GITHUB_TOKEN`. Tokens are never logged (redacted in `Debug`).

## Commands

```bash
ghr-stats collect        # the collector daemon (systemd service)
ghr-stats tui            # the interactive dashboard
ghr-stats github         # validate PAT(s); list each org's runners
ghr-stats db init        # create the DB + apply migrations
ghr-stats db prune --days 14   # drop time-series samples older than N days
```

`db prune` keeps `job_event` and is safe to run while the collector writes
(WAL). For full file reclamation after a large prune, `VACUUM` separately.
