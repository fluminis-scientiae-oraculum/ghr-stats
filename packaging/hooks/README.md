# Runner job hooks

`job-started.sh` and `job-completed.sh` are embedded in the binary. `sudo
ghr-stats config` (or the dashboard's `[h]`, as root) installs them into
`/var/lib/ghr-stats/hooks` and wires them into each runner's `.env`. Each appends one JSON line to
the runner's own event log, named by `GHR_STATS_EVENT_LOG`, and always exits 0.

Installing, chaining with existing hooks, the event format and how results are
matched: [Runner hooks](../../docs/hooks.md).
