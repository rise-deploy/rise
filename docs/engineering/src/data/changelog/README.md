# Changelog data

Each YAML file is one release: `0.22.0.yaml`, `unreleased.yaml`, or `planned.yaml`.
These files supply the browser changelog, individual release pages, deprecated
features, Markdown exports, and release upgrade notes. Install the docs
prerequisites with `npm ci --prefix docs` before running the release scripts.

A file needs a `version` and flat `changes` list. Released versions also need a
UTC `date` in `YYYY-MM-DD` format. Use literal blocks for Markdown descriptions
and migration instructions:

```yaml
version: unreleased
changes:
  - id: ingress-hostname-validation
    title: Ingress templates require bare hostnames
    impact: breaking
    backends: [kubernetes, docker, ecs]
    details: |-
      Ingress templates contain a bare hostname, without a scheme or path.

      See the configuration reference for the supported placeholders.
    action: |-
      Configure a subdomain template and wildcard DNS before upgrading.
```

- `id` is globally unique and stays with the change when it is released.
- `type` defaults to `change`. Other types are `feature`, `fix`, `maintenance`,
  `limitation`, and `deprecation`.
- `impact` defaults to `none`, or `action-required` for a deprecation. It can also
  be `breaking` or `configuration`.
- `backends` defaults to `[all]`. Specific values are `kubernetes`, `docker`, and
  `ecs`; `[all]` means the change affects every backend.
- `details`, optional `action`, and optional `migration` accept Markdown.
- `title` and `introduction` at the file level are optional. They describe the
  release; the file's `version` determines its lifecycle status.

`planned.yaml` contains work that is not yet merged. An optional `targetVersion`
on a planned entry records its intended full release. A planned target does not
make the change released or promise a release date.

When planned work merges, move its entry into `unreleased.yaml`, keep its `id`,
and remove `targetVersion`. The release containing the entry supplies its
version and date.

Optional `groups` contain heading metadata (`id`, `title`, and an optional
Markdown `introduction`). An entry's `group` references one of these IDs. Entries
without a group appear in the default changes group.

## Deprecated features

A deprecation is a normal change with a feature identifier and lifecycle stage:

```yaml
version: unreleased
changes:
  - id: raw-external-token-deprecation
    title: Raw external service-account tokens are deprecated
    type: deprecation
    feature: raw-external-service-account-tokens
    stage: deprecated
    featureTitle: Service-account authentication with raw external JWTs
    availability: |-
      The compatibility setting allows this authentication path.
    details: |-
      Accepted raw external-token requests emit a deprecation signal.
    migration: |-
      Set RISE_IDENTITY to the service account's email so the CLI exchanges
      the external token before making API calls.
```

A `deprecated` stage needs migration instructions. Later milestones use the same
`feature` ID and one of `introduced`, `default-disabled`, or `removed`. They can
provide updated migration instructions or inherit the implemented deprecation's
instructions. `firstCandidate` can record a verified prerelease `version` and
`date`. A milestone inherits its full-release version/date from the containing
file; planned milestones remain planned. A default change does not imply
removal. The deprecated-features page derives this lifecycle from the release
entries; there is no separate registry.

## Publishing

`node --test docs/scripts/changelog-data.test.mjs` checks YAML parsing and
release behavior. `node docs/scripts/changelog-release.mjs --unreleased` renders
the current upgrade notes as Markdown.

`scripts/changelog-release.sh <version> <release-notes-file> [date]` publishes a
full release: it writes `<version>.yaml` from the upcoming changes, creates the
matching MDX page, and clears `unreleased.yaml`. Planned changes stay planned.
Comments and literal Markdown blocks are preserved. The command refuses to
overwrite an existing release. `scripts/tag-version.sh` invokes it for full
releases.
