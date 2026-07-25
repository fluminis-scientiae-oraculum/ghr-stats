# Metrics

[← README](../README.md)

The collector can also expose the fleet metrics (both off by default; enable in
`[metrics]`). These are Persistent-mode features — they need the service:

- **Pull** — a tiny `/metrics` endpoint in Prometheus text format, bound to
  **`127.0.0.1:9477`** by default. The metrics are unauthenticated, so the bind
  address must stay on loopback. Always the literal `127.0.0.1`, never
  `localhost`.
- **Push** — periodically POSTs the metrics as JSON to an ingest endpoint (e.g.
  OpenObserve's `_json` API), with an optional `auth` header and an interval.

## Local liveness vs GitHub's view

ghr-stats holds two independent facts about every runner, and keeping them
separate is deliberate:

- **Local liveness** (`ghr_runner_up`, `ghr_fleet_by_state`) comes from
  inspecting the runner user's processes. It answers "is the listener running on
  this host".
- **GitHub's view** (`ghr_runner_github_online`) comes from the API reconcile. It
  answers "will GitHub give this runner work".

These can disagree, and the disagreement is the interesting state: a runner whose
listener is perfectly healthy but which GitHub will not dispatch to is
**divergent**. `ghr_runner_divergent` and `ghr_fleet_by_state{state="divergent"}`
name it.

> `divergent` **cross-cuts** `busy`/`idle`/`offline` rather than partitioning
> them — a divergent runner is still counted as idle or busy. Do not sum all
> four; you would double-count.

Alert on **duration**, never on the instantaneous bit. In the incident that
motivated these metrics the raw value flapped 62 times in three hours before
settling into a sustained failure; anything keyed to the instantaneous value
would have fired, been muted, and then missed the real outage.
`ghr_runner_github_offline_seconds` is the debounced quantity — it comes from a
persisted edge, so it survives collector restarts and scrape gaps.

### Reconcile health

`ghr_runner_github_online` being absent is not the same as it being `0`. An org
whose token broke, or which has no PAT at all, cannot be asked — and a scrape
must be able to tell that from "GitHub says offline":

| Metric | Meaning |
| --- | --- |
| `ghr_api_reconcile_ok{org}` | 1 if the last attempt for this org succeeded |
| `ghr_api_reconcile_timestamp_seconds{org}` | last **successful** reconcile |
| `ghr_api_reconcile_errors_total{org,kind}` | classified failure (`http_403`, `transport`, …) |
| `ghr_api_org_configured{org}` | 0 when no PAT is configured for the org |
| `ghr_runner_github_sample_age_seconds` | age of each runner's GitHub reading |
| `ghr_api_max_age_seconds` | the configured freshness window itself |

Past `ghr_api_max_age_seconds` a reading is treated as stale and its
`ghr_runner_github_online` series drops out rather than reporting an aged value
as current. Tune with `intervals.api_max_age_secs`.

> **Personal-account runners never reconcile, by design.** The reconcile calls
> `/orgs/{org}/actions/runners`, which is gated by an *organization*-scoped
> fine-grained PAT permission. A repository-level runner under a personal
> account has no equivalent permission to grant, so it reports
> `ghr_api_org_configured 0` permanently and has **no** `github_*` series at
> all. That is "we cannot ask", not "it is down" — which is why the
> `configured == 1` clause in the org alert below is required, and why the
> per-runner alerts are safe: a runner with no GitHub reading emits no
> `ghr_runner_github_offline_seconds`, so nothing can fire on it.

## Alert recipes

Sized against the measured flap. The third rule is what makes the first two
trustworthy — without it a dead reconcile presents as a calm fleet.

```yaml
- alert: GhrRunnerDivergent            # local healthy, GitHub says unusable
  expr: ghr_runner_github_offline_seconds > 900 and ghr_runner_up == 1
  for: 5m
  annotations:
    summary: "{{ $labels.name }} ({{ $labels.org }}) offline to GitHub for >15m while running locally"

- alert: GhrOrgAllRunnersOffline       # the org-wide pattern
  # The `configured == 1` clause is required, not optional. An org with no PAT
  # reports github_online = 0 forever, because we never asked — not because its
  # runners are down. Personal-account (repository-level) runners are the common
  # case: they have no org-scoped "Self-hosted runners" permission to grant, so
  # they can never be reconciled and would otherwise alert permanently.
  expr: |
    ghr_org_runners{state="github_online"} == 0
      and ghr_org_runners{state="total"} > 0
      and on(org) ghr_api_org_configured == 1
  for: 15m

- alert: GhrApiReconcileStale          # do not trust the two above without this
  expr: time() - ghr_api_reconcile_timestamp_seconds > 600
  for: 10m
```

For the push path, the equivalent is a scheduled search over the `runner`
records on `github_online = false AND up = 1`, grouped by `org`, with a `>15m`
sustain condition. The pushed records also carry `divergent`,
`github_offline_seconds` and a per-record `verdict`, so a query does not have to
re-derive the join.
