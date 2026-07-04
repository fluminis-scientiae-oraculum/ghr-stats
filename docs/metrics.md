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
