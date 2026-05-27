---
title: "Loki Backend"
---

The Loki backend keeps Rise as the only client-facing endpoint: the
frontend and CLI still call the Rise API, and Rise enforces project
authorization before issuing the query to Loki. Retention is configured
in Loki itself; `retention_hint` is only used to explain empty results
in the UI and CLI.

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
```

Rise scopes every query to a single deployment by selecting on two
stream labels — `project` and `deployment_id` by default. The pair must
uniquely identify a deployment's log stream; `deployment_id` is already
unique within a project, so no further uniqueness label is required.

If your Loki/Alloy stack already labels log lines with different names
(e.g. an operator-managed stack that uses `app` / `instance`), set
`labels.project` / `labels.deployment_id` to match. Names must be valid
LogQL identifiers (`[a-zA-Z_][a-zA-Z0-9_]*`); the backend rejects
invalid names at startup.

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
backend selects on, so query and ingest stay in sync automatically.

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
