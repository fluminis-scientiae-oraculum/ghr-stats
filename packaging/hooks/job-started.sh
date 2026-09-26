#!/usr/bin/env bash
# ghr-stats runner hook: JOB_STARTED. Appends one NDJSON line to this runner's
# own event log, which the collector tails; see docs/hooks.md. Unset
# GHR_STATS_EVENT_LOG means ghr-stats did not wire this runner: do nothing.
# Must never fail the job, so every step is best-effort and it always exits 0.

log="${GHR_STATS_EVENT_LOG:-}"
[ -n "$log" ] || exit 0
ts="$(date +%s 2>/dev/null || echo 0)"

printf '{"phase":"started","ts":%s,"repo":"%s","run_id":%s,"run_attempt":%s,"job":"%s","runner":"%s"}\n' \
  "$ts" "${GITHUB_REPOSITORY:-}" "${GITHUB_RUN_ID:-0}" "${GITHUB_RUN_ATTEMPT:-1}" \
  "${GITHUB_JOB:-}" "${RUNNER_NAME:-}" \
  >>"$log" 2>/dev/null || true

exit 0
