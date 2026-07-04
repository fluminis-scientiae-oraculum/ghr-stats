# Design & internals

[← README](../README.md)

## Architecture

- **Fully synchronous, no async runtime.** The collector is a handful of producer
  threads (local sampler, GitHub reconcile, hook-log tail) feeding a single
  SQLite-writer thread over a `crossbeam-channel`; metrics and the TUI's IPC read
  on their own WAL connections. The TUI↔collector link is a length-prefixed JSON
  protocol over a Unix socket — no HTTP framework, no runtime.
- **Single writer, DB-agnostic client.** The collector is the sole DB writer and
  reader; the TUI never opens the database — in Ephemeral mode it samples
  in-memory, in Persistent mode it reads through the socket.
- **Identity from `.runner`, never from systemd unit names.** A runner's
  locally-unique identity is its install directory; GitHub's `agentId` is unique
  only *within* an org, so the API view joins on `(org, agentId)`.
- **Two truths per runner** — local processes and the GitHub API, shown side by
  side so disagreement is visible.

## Platform

Linux only, today. Runner liveness/CPU/memory come from procfs + cgroup v2, the
host sampler reads `/sys` + `statvfs`, the collector's IPC uses an `AF_UNIX`
socket, and `systemd` manages the service — so the build fails fast on other
platforms rather than shipping something that can't sample. A thinner macOS build
(launchd + Mac process introspection) is future work.
