# Runner hooks

[← README](../README.md)

The Jobs/Detail job data comes from the runner's
`ACTIONS_RUNNER_HOOK_JOB_STARTED`/`_COMPLETED` hooks, which append one NDJSON
line per job to a shared event log that the collector tails. A runner allows
**exactly one** script per hook variable, and many operators already use them — so
ghr-stats **never clobbers**. Per runner it:

- **unset** → installs its script and sets the variable;
- **foreign hook** → offers, per runner, to **chain** (wrap the existing script,
  preserving its exit code, then append its own line) or **instruct** (print the
  exact snippet to add yourself);
- **ours** → idempotent no-op.

Scripts always `exit 0` (a non-zero `JOB_STARTED` fails the job). The shared log
must be writable by every runner user and readable by the collector — see
[`packaging/hooks/README.md`](../packaging/hooks/README.md).
