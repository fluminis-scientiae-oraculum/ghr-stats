#!/usr/bin/env bash
# ghr-stats runner hook — JOB_STARTED.
#
# Appends ONE NDJSON line to the shared event log that the ghr-stats collector
# tails. This runs as the *runner* user, so the log must be writable by every
# runner user and readable by the collector user (see packaging/hooks/README.md).
#
# It MUST NOT fail the job: every step is best-effort and the script always
# exits 0.

log="${GHR_STATS_EVENT_LOG:-/var/lib/ghr-stats/events.ndjson}"
ts="$(date +%s 2>/dev/null || echo 0)"

printf '{"phase":"started","ts":%s,"repo":"%s","run_id":%s,"run_attempt":%s,"job":"%s","runner":"%s"}\n' \
  "$ts" "${GITHUB_REPOSITORY:-}" "${GITHUB_RUN_ID:-0}" "${GITHUB_RUN_ATTEMPT:-1}" \
  "${GITHUB_JOB:-}" "${RUNNER_NAME:-}" \
  >>"$log" 2>/dev/null || true

exit 0
