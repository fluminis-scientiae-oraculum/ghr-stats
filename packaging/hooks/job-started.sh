#!/usr/bin/env bash
# ghr-stats runner hook — JOB_STARTED.
#
# Appends ONE NDJSON line to this runner's OWN event log, which the ghr-stats
# collector tails. The installer sets GHR_STATS_EVENT_LOG in the runner's .env to
# a file in the runner's install dir — owned by this (runner) user, so the append
# always succeeds; the collector reads it as root (see packaging/hooks/README.md).
#
# It MUST NOT fail the job: every step is best-effort and the script always
# exits 0.

# The installer sets GHR_STATS_EVENT_LOG to THIS runner's own log (in its install
# dir) — the exact path the collector tails. If it's unset, the runner wasn't
# wired by ghr-stats: do nothing rather than write a log the collector never reads.
log="${GHR_STATS_EVENT_LOG:-}"
[ -n "$log" ] || exit 0
ts="$(date +%s 2>/dev/null || echo 0)"

printf '{"phase":"started","ts":%s,"repo":"%s","run_id":%s,"run_attempt":%s,"job":"%s","runner":"%s"}\n' \
  "$ts" "${GITHUB_REPOSITORY:-}" "${GITHUB_RUN_ID:-0}" "${GITHUB_RUN_ATTEMPT:-1}" \
  "${GITHUB_JOB:-}" "${RUNNER_NAME:-}" \
  >>"$log" 2>/dev/null || true

exit 0
