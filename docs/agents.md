# ghr-stats for agents

[← README](../README.md)

Six verbs are **machine-facing**: they write a stable payload to stdout, encode
their answer in the exit code, and never colour or decorate it. Everything else
(`tui`, `config`, `systemd`, `db`, `uninstall`, `serve`) is for humans and
operators.

| Verb | Answers | Blocks? |
| --- | --- | --- |
| [`status`](#status) | Is the fleet healthy **now**? | no |
| [`explain`](#explain) | **Why** isn't it? | no |
| [`timeline`](#timeline) | **What changed**, and in what order? | no |
| [`doctor`](#doctor) | Is the **tool itself** wired up correctly? | no |
| [`wait`](#wait) | Block until the fleet reaches a state. | yes |
| [`tail`](#tail) | Follow transitions as they happen. | yes |

Start with `doctor`. Every other verb assumes the install is sound, and `doctor`
is the one that checks it.

## Exit codes

One table across every verb, so a caller can branch on the code without knowing
which verb produced it.

| Code | Meaning |
| --- | --- |
| `0` | The question was answered affirmatively. |
| `1` | Answered, and the answer is bad — degraded, or the wait timed out. |
| `2` | **Cannot determine.** Not "no": we could not see. |
| `3` | Usage or configuration error — the invocation itself was wrong. |

`2` is the one that matters. It is never a fact about the fleet; it is a fact
about our ability to observe it. Treating it as a `1` is the single most likely
way to build something that reports a false outage.

**Verbs that judge return a verdict; verbs that retrieve return availability.**
`status`, `explain` and `doctor` assess health, so `0` from them means *healthy*.
`timeline`, `wait` and `tail` make no health claim — `0` means only that the
question was answerable. So `ghr-stats timeline && echo healthy` is a bug: an
empty window is a perfectly good answer about a fleet that may be on fire.

`3` is deliberately not `2`: clap's default for a usage error is `2`, which would
collide with "cannot determine". A caller must never confuse a mistyped flag with
an unknowable fleet.

## Output contract

- **`--json` writes only the payload to stdout.** Diagnostics, progress and
  warnings go to stderr. Log output is disabled entirely for these verbs, so a
  stray `RUST_LOG` cannot corrupt what you parse.
- **`schema_version`** is on every JSON payload and starts at `1`. Fields may be
  added within a version; existing fields will not change meaning. Parse
  defensively — ignore unknown fields.
- **Times appear twice**: ISO-8601 UTC (`generated_at`) and epoch seconds
  (`generated_at_epoch`). No local time, no localised formats.
- **Unknown is `null`, never invented.** A runner with no readable GitHub view
  reports `github_online: null` — not `false`. The distinction is load-bearing
  everywhere in this tool.
- **No ANSI, ever.** These verbs emit no escape sequences at all — not
  conditionally on a TTY, not suppressed by `NO_COLOR`, simply never written. So
  there is no terminal-detection behaviour to depend on, and piping changes
  nothing about the bytes. (Colour belongs to the dashboard, which owns its own
  screen.) No thousands separators, no localised numbers.

### The SQLite schema is not an interface

The database under `db_path` is an implementation detail. Its tables have been
re-keyed twice already (`runner_state` moved from `agent_id` to the install
`dir`, because GitHub's `agent_id` is unique only *within an org* and this fleet
has a real collision). Query the verbs. They are versioned; the schema is not.

## status

```bash
ghr-stats status --json [--org ORG] [--runner NAME]
```

The fleet as one payload: per-runner liveness, GitHub's view of each runner,
per-org reconcile health, and a `verdict`. Filtering recomputes the verdict over
the surviving rows, so `--org healthy-org` does not inherit another org's
"degraded".

With no collector it falls back to a live local scan, reports
`mode: "ephemeral"`, and sets every `github_*` field to `null` — a local scan can
see processes, never GitHub.

## explain

```bash
ghr-stats explain --json
```

Findings, worst first. Each carries a `claim`, the `evidence` it rests on,
`suggested_checks`, and — the load-bearing field — a **`boundary`**:

| `boundary` | Investigate |
| --- | --- |
| `local` | This host: the runner process, its unit, its disk. |
| `github` | GitHub's side: the org's Actions service, permissions, a shard. |
| `network` | Between the two: egress, DNS, a proxy. |
| `config` | Our own configuration: a missing PAT, an unknown org. |

Establishing which side of the fence a fault sits on was the most expensive part
of the incident that motivated this tool, and it is the part a fleet monitor can
shortcut — it holds the local process truth and GitHub's opinion at the same
instant.

`severity` ranks how *invisible* a problem is, not how loud. `github-divergence`
outranks a plainly offline runner because every other surface already shows the
offline one in red, while divergence reads as green everywhere.

## timeline

```bash
ghr-stats timeline --since 6h [--org ORG] [--runner NAME] [--limit N] [--samples] [--json]
```

The window as the things that **changed** in it, not as the samples underneath.
Four streams, kept separate because their disagreement is the diagnosis:

- local liveness edges (a process fact),
- GitHub-online edges (a remote opinion),
- per-org reconcile outcomes (whether we could hold that opinion at all),
- job starts and completions.

"Eight runners went GitHub-offline" means something entirely different depending
on whether the reconcile was still succeeding at the time. That is why they are
never collapsed.

Bounded by construction: `--since` is capped at 7 d, `--limit` (default 500)
applies per section, and every section reports `limited` when it was cut. A
window reaching past what `db prune` has left reports `truncated_at` — where the
record *starts*, deliberately not why, because a pruned history and a young one
are indistinguishable from here.

> **Known limit.** `transitions` merges the liveness, GitHub and reconcile
> streams under one `--limit`, keeping the newest across all three. A stream that
> flaps hard can therefore crowd out a quieter one within the same window — and
> the flapping case is exactly an incident. When `limited` is true, narrow with
> `--org` / `--runner` or shorten `--since` rather than trusting the mix. Job
> edges are bounded separately and are not affected.

## doctor

```bash
ghr-stats doctor [--json] [--offline]
```

Preflights the install: config parses, each org's PAT can still list its runners,
hooks are installed, the collector is reachable **and is the same build as this
binary**, the database exists, and where the retained record starts.

Three outcomes per check — `pass`, `fail`, `skipped` — and the third is the point.
**A check that could not run is never reported as passing**, and any skip holds
the verdict at `2`. The common case is real: the system config is `0600 root`, so
a non-root `doctor` genuinely cannot inspect PATs and says so. Re-run with `sudo`
for the full picture.

Every `fail` carries a `fix` — a concrete next command. `--offline` skips the one
check that calls GitHub, and skipping keeps the verdict at `2` rather than green.

## wait

```bash
ghr-stats wait --github-online [--org ORG] [--timeout 600] [--json]
```

Blocks until every runner in scope is online to GitHub. This replaces the
`while ! ghr-stats status; do sleep 30; done` loop — which gets three things
wrong that this does not:

- **A timeout while the GitHub view was unreadable exits `2`, not `1`.** Our
  blindness must never be reported as the fleet's answer.
- **A filter matching no runners exits `2`, not `0`.** "Every runner in the empty
  set is online" is vacuously true, so `--org typo` would otherwise report
  success.
- **With no collector it exits immediately**, not after the full timeout. The
  GitHub view lives only there.

Polls at the local sampling interval. Progress goes to stderr and only when it
changes; the final snapshot goes to stdout on every outcome. `--timeout 0`
evaluates once.

## tail

```bash
ghr-stats tail [--org ORG] [--runner NAME] [--backfill SECONDS]
```

Every transition as one JSON object per line, flushed per line — NDJSON, safe to
pipe into `jq` or an agent loop. Starts from now; `--backfill` replays a window
first.

```json
{"type":"transition","ts":1785044382,"at":"2026-07-26T05:39:42Z","org":"acme","edge":{"liveness":{"runner":"r1","from":"busy","to":"offline"}}}
{"type":"job","ts":1785044387,"at":"2026-07-26T05:39:47Z","org":"acme","runner":"r1","repo":"acme/web","job":"build","edge":{"completed":{"conclusion":"success"}}}
```

Branch on `type`: `transition`, `job`, or **`gap`**.

```json
{"type":"gap","section":"transitions","since_epoch":1785044340,"until_epoch":1785044400,"limit":500}
```

A `gap` means more transitions occurred in that window than one poll could carry,
so **events were missed** — re-ask `timeline` for the named window. It is emitted
*before* the events it qualifies, so a consumer never acts on a batch it believes
is complete and learns otherwise afterwards. Silence about falling behind would
be indistinguishable from calm, which is the failure this whole tool exists to
prevent.

`tail` **polls; it does not subscribe**, and that is a decision rather than a
shortcut. A transition does not exist until a sampler observes it, so a
subscription would deliver the same events at the same moments — while holding
one of the collector's few connection slots for its entire life, which is how a
handful of forgotten streams would lock every other client out. Polling also lets
it prove it kept up, which is what the `gap` line is.

A job's `conclusion` is `null` until the reconcile resolves it from the API: the
hook knows a job ended, not whether it passed. You see the completion once, with
an unknown outcome, and no correction follows — ask `status` or the job queries
for the settled answer.

Ends with `0` when its reader closes (`ghr-stats tail | head -5`), or `2`
immediately if there is no collector. Ctrl-C ends it at `130`, the shell
convention.

## Collector versus binary

Every verb that needs history or GitHub's view needs the collector, and the two
must be the **same build**. After upgrading:

```bash
sudo systemctl restart ghr-stats.service
```

Until you do, the wire versions disagree and the verbs report `version-drift` and
exit `2` rather than answering from a stale or mismatched source. `doctor` names
this explicitly, including the quieter case where the wire versions still match
but the builds differ.
