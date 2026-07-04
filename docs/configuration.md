# Configuration

[← README](../README.md)

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
[`config.example.toml`](../config.example.toml) for every field.

## Editing config from a non-root TUI (the `ghr-stats` group)

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

## GitHub API (optional, read-only)

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
