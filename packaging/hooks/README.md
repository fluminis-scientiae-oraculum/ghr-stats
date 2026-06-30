# Runner job hooks

These two scripts make each runner record job start/completion to the shared
NDJSON event log that the `ghr-stats` collector tails to populate the Jobs view.
They are best-effort and always exit 0 — they can never fail a job.

## Shared event log

Hooks run as the **runner** user; the collector runs as the **operator**. The
log must therefore be writable by every runner user and readable by the
operator. Single-line appends are atomic on a local filesystem, so concurrent
runners interleave safely. Example one-time setup:

```bash
sudo install -d -m 0775 -o root -g ghr /var/lib/ghr-stats   # a group all runners share
sudo touch /var/lib/ghr-stats/events.ndjson
sudo chgrp ghr /var/lib/ghr-stats/events.ndjson
sudo chmod 0664 /var/lib/ghr-stats/events.ndjson
```

Point ghr-stats at it in `config.toml`:

```toml
event_log = "/var/lib/ghr-stats/events.ndjson"
```

(Override per runner with `GHR_STATS_EVENT_LOG` if you prefer.)

## Wiring (in fleet-scripts' runner env)

Install the scripts somewhere the runner users can execute, then set the
runner's hook env vars (e.g. in its `.env`):

```bash
ACTIONS_RUNNER_HOOK_JOB_STARTED=/opt/ghr-stats/hooks/job-started.sh
ACTIONS_RUNNER_HOOK_JOB_COMPLETED=/opt/ghr-stats/hooks/job-completed.sh
```

The collector fills job **timing** from these events; the **conclusion**
(success/failure) is reconciled from the GitHub API.
