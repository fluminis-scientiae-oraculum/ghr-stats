# Runner job hooks

These two scripts make each runner record job start/completion to an NDJSON event
log that the `ghr-stats` collector tails to populate the Jobs view. They are
best-effort and always exit 0 — they can never fail a job.

## Per-runner event log (no shared file)

Each runner writes its **own** log, a dotfile in its install-dir root:

```
<runner-install-dir>/.ghr-stats-events.ndjson
```

That directory is owned by the **runner** user, so the hook (which runs as that
user) can always create and append to it — no shared file, no group setup, no
`chmod` of a root-owned directory. The collector runs as root and reads every
runner's log, tailing each independently (one byte offset per log). Single-line
appends are atomic, so a runner's start/completion writes never tear.

`ghr-stats config` (or the TUI `[h]` action) wires this automatically: it writes
`GHR_STATS_EVENT_LOG=<runner-install-dir>/.ghr-stats-events.ndjson` into the
runner's `.env` alongside the hook vars. If the var is unset the scripts do
nothing (a runner that wasn't wired by ghr-stats).

## Wiring (what the installer writes into the runner's `.env`)

```bash
ACTIONS_RUNNER_HOOK_JOB_STARTED=/var/lib/ghr-stats/hooks/job-started.sh
ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/var/lib/ghr-stats/hooks/job-completed.sh
GHR_STATS_EVENT_LOG=/srv/actions-runner/runner-01/.ghr-stats-events.ndjson
```

The collector derives the same per-runner path from each discovered runner's
install dir, so the writer and the reader can never point at different files.

The collector fills job **timing** from these events; the **conclusion**
(success/failure) is reconciled from the GitHub Actions API on the next reconcile
cycle — but only when the org's PAT also carries the fine-grained **"Actions:
read"** repository permission. A runners-only token can't read run jobs, so
conclusions simply stay a neutral "done" (timing still shows).
