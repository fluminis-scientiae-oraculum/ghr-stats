#!/usr/bin/env bash
# ghr-stats runner hook — JOB_COMPLETED.  See job-started.sh for the contract.
# MUST NOT fail the job: best-effort, always exits 0.

log="${GHR_STATS_EVENT_LOG:-/var/lib/ghr-stats/events.ndjson}"
ts="$(date +%s 2>/dev/null || echo 0)"

printf '{"phase":"completed","ts":%s,"repo":"%s","run_id":%s,"run_attempt":%s,"job":"%s","runner":"%s"}\n' \
  "$ts" "${GITHUB_REPOSITORY:-}" "${GITHUB_RUN_ID:-0}" "${GITHUB_RUN_ATTEMPT:-1}" \
  "${GITHUB_JOB:-}" "${RUNNER_NAME:-}" \
  >>"$log" 2>/dev/null || true

exit 0
