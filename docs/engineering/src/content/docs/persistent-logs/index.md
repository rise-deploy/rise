---
title: "Persistent Logs"
---

Rise exposes deployment logs through its API, frontend, and CLI. Two
backends are available, selected by the `deployment_logs.type` setting:

| Backend | What it queries | Historical reach |
|---|---|---|
| `kubernetes` (default) | `pods/log` on the active Pod | Only while the Pod exists |
| `loki` | Grafana Loki via HTTP | As long as Loki retains the lines |

Regardless of the backend, the Rise API is the only thing clients talk
to — Rise enforces project authorization before reading from the backing
log store. Loki (or the Kubernetes API) is never exposed to end users.

## Kubernetes backend (default)

```yaml
deployment_logs:
  type: kubernetes
```

This is the default and requires no extra infrastructure. It streams
the Kubernetes `pods/log` API directly.

Limitations:

- **No history past the Pod's lifetime.** When a deployment is rolled,
  superseded, or its Pod is evicted, its logs vanish. The UI and CLI
  surface this as "no active deployment pod was found."
- **Backward scrolling is bounded by the kubelet's per-container log
  retention.** The UI can page back through whatever the kubelet
  currently holds (typically a few MB per container, controlled by
  `--container-log-max-size`), and stops at "Start of selected range"
  once that buffer is exhausted.
- **No cross-Pod aggregation.** Each Pod's stream is independent; there
  is no merged view across replicas (only a single Pod's lines are
  returned per request).
- **No log volume chart.** The chart in the UI requires a metric-style
  count query that Loki provides; the Kubernetes backend reports
  "Log volume charts are only available with Loki."

For any of the above, switch to the Loki backend.

## Loki backend

See [Loki backend](./loki/) for setup with the bundled Helm subchart, an
operator-managed external Loki, label overrides, and bearer-token wiring.
