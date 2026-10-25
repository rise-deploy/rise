---
title: "Loki Backend"
---

The Loki backend keeps Rise as the only client-facing endpoint: the
frontend and CLI still call the Rise API, and Rise enforces project
authorization before issuing the query to Loki. Retention is configured
in Loki itself; `retention_hint` is only used to explain empty results
in the UI and CLI.

:::caution[Requires Loki 3.0+]
Rise's volume chart (`sum by (detected_level) (count_over_time(...))`)
and the server-side `?level=` filter both push `detected_level` into
LogQL — without it, the chart and any level-filtered query come back
empty. Per-line classification on the unfiltered line list also
prefers `detected_level` when present, but falls back to the same
regex the Kubernetes backend uses when Loki hasn't classified an
entry yet (in-flight entries on the WS tail are emitted before the
classifier hits the chunk), so the live tail is never dim by default.
The bundled Loki subchart already pins a 3.x release, so this only
affects operators pointing Rise at an external Loki: verify your
deployment runs Loki 3.0 or later.
:::

## Available levels

Loki advertises eight `detected_level` values — `unknown, trace,
debug, info, warn, error, critical, fatal`. Rise mirrors that list
via `GET /api/v1/logs/capabilities`, which the frontend uses to drive
the filter dropdown and the chart's per-level palette. The endpoint
also reports `supports_volume: false` for the Kubernetes backend so
clients hide the volume panel rather than waiting for a 404.

## Backend configuration

```yaml
deployment_logs:
  type: loki
  url: "http://rise-loki:3100"
  tenant_id: "tenant-a"              # optional X-Scope-OrgID
  bearer_token_env: "RISE_LOKI_TOKEN" # optional backend-only token
  retention_hint: "7d"               # optional display-only hint
  labels:                            # optional — override Loki label names
    project: "rise_project"          #   defaults shown
    deployment_id: "rise_deployment_id"
    container: "container"
```

Note the casing difference between the two surfaces: the backend YAML
uses snake_case keys (`labels.deployment_id`), while the Helm values
expose the same setting as `logs.loki.labels.deploymentId` (camelCase,
because Helm renders the Rust struct's serde alias). Both ultimately set
the same Loki label name — copy snippets carefully when moving between
`/etc/rise/local.yaml` and `--set` / `values.yaml`.

Rise scopes every query to a single deployment by selecting on two
stream labels — `project` and `deployment_id` by default. The pair must
uniquely identify a deployment's log stream; `deployment_id` is already
unique within a project, so no further uniqueness label is required.

`container` is the third label Rise reads. It carries the deployment's
container name (`app` for a single-container deployment) and backs both
the `?container=` filter and the per-line container attribution the logs
UI renders. A stack that does not emit it serves unattributed lines, and
a container filter then matches nothing.

If your Loki/Alloy stack already labels log lines with different names
(e.g. an operator-managed stack that uses `app` / `instance`), set
`labels.project` / `labels.deployment_id` / `labels.container` to match.
Names must be valid LogQL identifiers (`[a-zA-Z_][a-zA-Z0-9_]*`); the
backend rejects invalid names at startup.

## Bundled Loki + Alloy subcharts

The Helm chart can install Grafana Loki and Grafana Alloy as optional
subcharts. This is the simplest path for fresh installs:

```yaml
logs:
  backend: loki
  loki:
    enabled: true
    retentionHint: 7d
  alloy:
    enabled: true
```

The bundled Alloy is configured to scrape only Pods labelled
`app.kubernetes.io/managed-by=rise` and writes the same label names the
backend selects on, so query and ingest stay in sync automatically. The
Kubernetes container name it ships as `container` is the Rise container
name, because every container of a deployment runs in its own Pod.

### Alloy RBAC scope

The bundled Alloy watches Pods **cluster-wide** rather than restricting
its `discovery.kubernetes` to a single namespace. Rise spreads app
workloads across many namespaces (typically one per project/environment,
sharing a common prefix), and Alloy's `namespaces.names` field accepts
only literal names — no wildcards or regex. The cluster-wide watch is
paired with a relabel rule that keeps only Pods carrying
`app.kubernetes.io/managed-by=rise`, so non-Rise workloads are dropped
at ingest.

This means the bundled Alloy `ServiceAccount` needs cluster-wide
`get`/`list`/`watch` on Pods (the chart's RBAC reflects that). Operators
who require stricter RBAC scoping can disable the bundled shipper
(`logs.alloy.enabled: false`) and run their own log shipper — either
with a tighter per-namespace selector, or one deployment per namespace —
provided it emits the same Loki labels the backend queries on.

## Operator-managed external Loki

To point Rise at an existing Loki, leave the subchart disabled and set
`externalUrl`:

```yaml
logs:
  backend: loki
  loki:
    enabled: false
    externalUrl: "https://loki.example.internal"
    tenantId: "rise"
    bearerTokenSecret:
      name: loki-token
      key: token
  alloy:
    enabled: true
```

If you also run your own log shipper, set `logs.alloy.enabled: false`
and skip the `extraEnv` block below. The three supported topologies are
then: (a) bundled Loki + bundled Alloy, (b) external Loki + bundled
Alloy, and (c) external Loki + external shipper.

When you bring your own log shipper too, set
`logs.loki.labels.{project,deploymentId}` to match the labels your
shipper already emits — the backend can't query labels it doesn't know
about.

### Bearer tokens

`logs.loki.bearerTokenSecret` only wires the *backend*; the bundled
Alloy needs the same token to push. Add a matching `extraEnv` to the
Alloy subchart values:

```yaml
alloy:
  alloy:
    extraEnv:
      - name: RISE_LOKI_BEARER_TOKEN
        valueFrom:
          secretKeyRef:
            name: loki-token
            key: token
```

The chart's pre-install check fails fast if a bearer secret is set but
the matching Alloy `extraEnv` is missing, so this misconfiguration is
caught at `helm install` time rather than at first query.
