# Configuration

[← Docs](README.md)

Every setting has a default, so ghr-stats runs with no config file at all. The
file holds GitHub tokens, so there is no per-user copy: it lives at
`/etc/ghr-stats/config.toml`, owned by root, and ghr-stats writes it mode `0600`.
[`config.example.toml`](../config.example.toml) lists every field with comments.

## Which file

| Order | Source |
| --- | --- |
| 1 | `--config FILE` |
| 2 | `$GHR_STATS_CONFIG` |
| 3 | `/etc/ghr-stats/config.toml`, if it exists |
| — | none: every value is a default |

The wizard and the TUI write back to the file they read. An edit made through a
running collector lands in the collector's file.

A file that exists but cannot be read (the usual case for a non-root user) is
not the same as no file. Read-only commands and the dashboard carry on with
defaults and take live data from the collector, while anything whose outcome
depends on the real settings refuses and names the file: `serve`, `db prune`, and
the TUI's `[m]`. `doctor` reports the unread checks as skipped, never as passing.

A user-scope collector (`systemd install --user`) can only use a config its user
can read: either no `/etc` file, or a readable file named by `GHR_STATS_CONFIG`
in the unit's environment.

## Settings

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `db_path` | path | `/var/lib/ghr-stats/ghr-stats.db` as root, `$XDG_DATA_HOME/ghr-stats/ghr-stats.db` otherwise | the collector's SQLite database |
| `runner_roots` | list of paths | `[]`: parents of every `actions.runner.*` unit's working directory | where runner install dirs (each with a `.runner`) are found |
| `orgs` | list of logins | `[]`: every scope found in `.runner` files | github.com organizations to reconcile **instead of** the discovered scopes |
| `retention_days` | 1–65535, or `"forever"` | `30` | samples older than this are pruned hourly; job history is kept |
| `intervals.local_secs` | seconds | `5` | local sampling cadence |
| `intervals.api_secs` | seconds | `60` (floor 10) | GitHub reconcile cadence |
| `intervals.api_max_age_secs` | seconds | 3 × `api_secs`, at least 180 | how old a GitHub reading may be and still be served |
| `github.tokens` | table of key → PAT | empty | a PAT per org or account; see [below](#github-tokens) |
| `github.token` | PAT | none | github.com fallback for any owner without its own key |
| `metrics.pull.enabled` | bool | `false` | serve `/metrics` |
| `metrics.pull.addr` | `ip:port` | `127.0.0.1:9477` | where `/metrics` listens |
| `metrics.push.enabled` | bool | `false` | POST the snapshot as JSON |
| `metrics.push.endpoint` | URL | empty | where to POST |
| `metrics.push.auth` | string | none | sent verbatim as `Authorization` |
| `metrics.push.interval_secs` | seconds | `30` (floor 5) | push cadence |

`api_max_age_secs` bounds GitHub's view: past it a runner reads `stale` and its
`ghr_runner_github_online` series disappears rather than serving an old answer as
current. Three polls, not one, so a single slow cycle does not mark everything
stale. The window in force is exported as `ghr_api_max_age_seconds`.

Retention runs inside the collector in small batches between writes; `"forever"`
turns it off. To prune once by hand, or to reclaim disk space, see
[CLI & operations](cli.md#the-collector-service).

## GitHub tokens

Local sampling needs no token. A token adds GitHub's own view of each runner
(online, busy) and, optionally, each job's result. Use a **fine-grained**,
read-only PAT (`github_pat_…`): the wizard, the TUI and the collector's socket
refuse anything else, and `doctor` fails a token of another kind found in the
file.

| Runner registered to | Resource owner | Permission |
| --- | --- | --- |
| an organization | the organization | Organization → **Self-hosted runners: Read** |
| a repository | the repository's owner | Repository → **Administration: Read**, on the repositories with runners |
| an enterprise | — | not reconciled: GitHub requires a classic token |
| *any, for job results* | as above | Repository → **Actions: Read** (optional) |

"Repository access" must be **All** or **Only select repositories** for any
repository permission to appear; "Public repositories" exposes none.

Keys name the owner, and the host when it is not github.com:

```toml
[github.tokens]
"example-org" = "github_pat_..."               # github.com organization or account
"ghe.example.com/platform" = "github_pat_..."  # GitHub Enterprise Server
"acme.ghe.com/platform" = "github_pat_..."     # GHE.com
```

A malformed key rejects the config at load. Keys match owners
case-insensitively. `github.token` and `GHR_STATS_GITHUB_TOKEN` apply to
github.com only, so a github.com token is never sent to another host. Tokens are
never logged, never returned over the socket, and redacted wherever the config is
printed.

## Editing

| Surface | Needs | Changes |
| --- | --- | --- |
| `ghr-stats config` | write access to the config file (root for `/etc`); root for its hook step | runner roots, PATs (masked, validated before saving), metrics, hooks |
| TUI `[a]` PATs | root, or membership of the `ghr-stats` group with the collector running | the config, through the collector when one runs |
| TUI `[m]` pull metrics | a readable config (root for `/etc`), to know the current state; with a collector running, also root or the `ghr-stats` group | the config, through the collector when one runs |
| TUI `[h]` hooks | root | runner `.env` files |
| TUI `[o]` | write access to the file | opens it in `$EDITOR` as the dashboard's user |

`sudo ghr-stats systemd install --system` creates the `ghr-stats` group and adds
the invoking user. A member's `[a]` edits travel over the socket to the root
collector, which takes the caller's uid from the kernel and reads the group
database on each connection, so `usermod -aG ghr-stats <user>` takes effect
without logging in again.

## Environment

| Variable | Effect |
| --- | --- |
| `GHR_STATS_CONFIG` | config file to read and write |
| `GHR_STATS_GITHUB_TOKEN` | github.com PAT for owners without their own key; overrides `github.token` |
| `GHR_STATS_ALLOW_TTY` | lets `serve` run on a terminal (development) |
| `RUST_LOG` | log filter for `serve`, `config`, `systemd`, `db` and `uninstall` (stderr); the dashboard and the machine-facing verbs do not log |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR` | user-scope locations ([Design](design.md#where-things-live)) |

## See also

- [Runner hooks](hooks.md): what the Actions permission feeds
- [Metrics](metrics.md): the `[metrics]` outputs
- [Privileged operations](privileged.md): what `sudo` is for
