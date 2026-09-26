# ghr-stats for agents

[← Docs](README.md)

Six verbs are **machine-facing**: a stable payload on stdout, the answer in the
exit code, nothing decorated. The rest (`tui`, `config`, `systemd`, `db`,
`uninstall`, `serve`) are for operators.

| Verb | Answers | Blocks | `0` means |
| --- | --- | --- | --- |
| [`status`](#status) | Is the fleet healthy now? | no | healthy |
| [`explain`](#explain) | Why not? | no | healthy |
| [`doctor`](#doctor) | Is ghr-stats itself set up correctly? | no | healthy |
| [`timeline`](#timeline) | What changed, in what order? | no | answered |
| [`wait`](#wait) | Block until the fleet reaches a state | yes | the state was reached |
| [`tail`](#tail) | Follow transitions live | yes | the reader closed the pipe |

Start with `doctor`: every other verb assumes the install is sound.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Answered, and the answer is good. |
| `1` | Answered, and the answer is bad. |
| `2` | **Cannot determine**: not "no", but "could not see". |
| `3` | Usage or configuration error. |

| Verb | `0` | `1` | `2` |
| --- | --- | --- | --- |
| `status`, `explain` | runners found, none offline or divergent | a runner is offline, or divergent where GitHub's view is fresh | no runner in scope |
| `doctor` | every check passed | a check failed | a check was skipped and none failed |
| `timeline` | answered | — | no collector |
| `wait` | every runner online to GitHub | timed out while GitHub's view was readable | see [wait](#wait) |
| `tail` | the reader closed the pipe | — | no collector on the first poll |

An error the verb cannot work around is `2`. Usage errors are `3`, not clap's
usual `2`, so a typo never reads as an unknown fleet.

Treat `2` as "unknown", never as an outage. `status`, `explain` and `doctor` judge
health, so their `0` means healthy; `timeline` and `tail` only retrieve, so
`ghr-stats timeline && echo healthy` is a bug. Without a collector, `status` and
`explain` answer from a scan of this host and `doctor` fails its `collector`
check; see [Design](design.md#clients-and-modes).

## Output contract

- `--json` writes only the payload to stdout. Progress and diagnostics go to
  stderr, and these verbs emit no logs, so `RUST_LOG` cannot corrupt the stream.
- Every `--json` payload has `schema_version` (currently `1`); `tail` lines do
  not. Fields may be added within a version; existing fields keep their meaning.
  Ignore unknown fields.
- Times are UTC: ISO-8601 (`generated_at`), epoch seconds (`generated_at_epoch`,
  `since_epoch`), or both.
- An unknown GitHub view is `null`, never `false`. `state_seconds` is `0` when no
  liveness edge is recorded, which is always the case without a collector.
- No ANSI escapes, colours or localised numbers, whether or not stdout is a
  terminal.
- A closed pipe (`| head`) ends the verb cleanly.

Query these verbs, not the database: the SQLite schema is not an interface and
changes between releases.

## status

```bash
ghr-stats status --json [--org ORG] [--runner NAME]
```

The fleet as one payload: each runner's local liveness and GitHub's view of it,
per-org reconcile health, and a `verdict`. A filter recomputes counts and verdict
over the rows that remain, so a healthy org never inherits another org's
`degraded`.

An org with no fresh GitHub reading for any of its runners (no PAT, a broken
token, a reconcile gap) is `unknown`, not `degraded`. `reconcile_age_s` is
`null` for an org that has never reconciled; branch on it to tell "never asked"
from "asked recently".

Without a collector, `status` scans the host itself, reports `mode: "ephemeral"`
and sets every `github_*` field to `null`.

## explain

```bash
ghr-stats explain --json
```

Findings, worst first, each with a `claim`, the `evidence` behind it,
`suggested_checks`, and a `boundary` saying where to look.

| `boundary` | Investigate |
| --- | --- |
| `local` | this host: the runner process, its unit, its disk |
| `github` | GitHub's side: the org's Actions service, permissions |
| `network` | between the two: egress, DNS, proxy |
| `config` | ghr-stats' own setup: a missing PAT, no collector |

| Finding | Severity | Boundary | Raised when |
| --- | --- | --- | --- |
| `github-divergence` | high | `network` when more than one org has a GitHub reading and all of them are affected, else `github` | runners are up locally but offline to GitHub |
| `runners-offline-locally` | medium | `local` | a runner's listener is not running |
| `github-view-stale` | medium | `github` | an org reconciled before, but its runners have no fresh reading |
| `org-never-reconciled` | info | `config` | an org has runners here and has never reconciled |
| `github-view-unavailable` | info | `config` or `local` | there is no usable collector; the claim names why |

`severity` ranks how easily a problem hides, not how loud it is: divergence
outranks an offline runner because every other surface already shows the offline
one in red, while a divergent runner looks green.

## doctor

```bash
ghr-stats doctor [--json] [--offline]
```

| Check | Passes when | Skipped when |
| --- | --- | --- |
| `config` | the config file exists and parses (fails when missing or invalid) | it exists but is unreadable (re-run with `sudo`) |
| `collector` | a collector answers and is the same build as this binary | never: an absent or mismatched collector fails, with the fix |
| `reconcile` | every org that has reconciled did so within `api_max_age_secs` | no collector answered |
| `history` | the collector reports where its record starts | no collector answered |
| `runner-roots` | runners are found under the roots | the config was not loaded |
| `database` | the database file exists | the config was not loaded |
| `hooks` | every runner's hooks run ghr-stats, directly or through a chain wrapper | the config was not loaded, no runners were found, or a runner's `.env` is unreadable |
| `tokens` | each org's PAT lists its runners; at least one reconcilable org has a PAT (fails when there are no orgs at all) | `--offline`, or the config was not loaded |

Every `fail` carries a `fix`: a command to run next. A skipped check never counts
as passing: with no failure, a skip makes the verdict `2`.

## timeline

```bash
ghr-stats timeline --since 6h [--org ORG] [--runner NAME] [--limit N] [--samples] [--json]
```

The window as what **changed** in it, in four streams kept apart because their
disagreement is the diagnosis:

| Stream | Fact |
| --- | --- |
| local liveness edges | the runner's processes |
| GitHub-online edges | GitHub's opinion |
| per-org reconcile outcomes | whether that opinion could be fetched at all |
| job starts and completions | the hooks |

`--since` (default `6h`) needs a unit (`90s`, `30m`, `6h`, `2d`) and is capped at
7 days; the header shows the window actually used. `--limit` (default 500, at most
2000) applies per section and each section reports `limited` when cut. A window that reaches past
the oldest retained sample reports `truncated_at`, where the record starts.

`transitions` merges the first three streams under one `--limit` that keeps the
newest rows (printed oldest first), so a stream that flaps hard can crowd out a
quiet one. When `limited` is true,
narrow with `--org` / `--runner` or shorten `--since`. Jobs are bounded
separately.

## wait

```bash
ghr-stats wait --github-online [--org ORG] [--timeout 600] [--json]
```

Blocks until every runner in scope is online to GitHub, polling at the local
sampling interval. Progress goes to stderr when it changes; the final snapshot,
narrowed to `--org` when given, goes to stdout on every outcome. `--timeout 0`
evaluates once.

| Outcome | Exit |
| --- | --- |
| every runner online to GitHub | `0` |
| timed out while GitHub's view was readable | `1` |
| timed out while the view was unreadable | `2` |
| the filter matched no runners | `2` |
| no collector | `2`, immediately |

## tail

```bash
ghr-stats tail [--org ORG] [--runner NAME] [--backfill SECONDS]
```

Each transition as one JSON line, flushed per line. The first poll looks back
`max(4 × local_secs, 60 s)`; `--backfill` (up to 7 days) replays a longer window.

```json
{"type":"transition","ts":1785044382,"at":"2026-07-26T05:39:42Z","org":"example-org","edge":{"liveness":{"runner":"runner-01","from":"busy","to":"offline"}}}
{"type":"job","ts":1785044387,"at":"2026-07-26T05:39:47Z","org":"example-org","runner":"runner-01","repo":"example-org/web","job":"build","edge":{"completed":{"conclusion":null}}}
{"type":"gap","section":"transitions","since_epoch":1785044340,"until_epoch":1785044400,"limit":500}
```

Branch on `type`. A **`gap`** means one poll could not carry every event in that
window, so events may have been missed: re-ask `timeline` for it. It comes
*before* the events it qualifies.

`tail` polls the collector each local interval and holds no connection between
polls. If the collector goes away after the first poll (a restart), `tail` says
so on stderr, keeps its window, and resumes when the collector returns. A job's
`conclusion` is `null` unless the reconcile resolved it before `tail` saw the
completion; `tail` never re-emits a job to add it.

| Outcome | Exit |
| --- | --- |
| the reader closed the pipe | `0` |
| no collector on the first poll | `2` |
| Ctrl-C | killed by SIGINT (a shell reports `130`) |

## After an upgrade

Clients and the collector must speak the same IPC version. Until the service is
re-installed from the new binary ([CLI & operations](cli.md#the-collector-service)),
`status` and `explain` answer from a scan of this host, `timeline`, `wait` and
`tail` exit `2`, and `doctor` fails its `collector` check with the fix.

## See also

- [Metrics](metrics.md): the same snapshot for Prometheus
- [CLI & operations](cli.md): every command
- [Design](design.md#clients-and-modes): how a verb finds the collector
