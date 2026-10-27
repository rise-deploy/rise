---
title: "Upcoming Changes"
---

Changes that are planned but not merged yet, with the release each is due in
where that is decided. Use this page to prepare an install ahead of time; the
[Upgrade Notes](/operator-docs/upgrade-notes/) cover what a release actually
ships. Badges follow the Upgrade Notes'
[impact legend](/operator-docs/upgrade-notes/#impact-legend).

Each change is tracked by a
[`rollout-gate` issue](https://github.com/rise-deploy/rise/issues?q=is%3Aissue+is%3Aopen+label%3Arollout-gate).
When it merges, its entry moves from this page to the Unreleased section of the
Upgrade Notes.

## 0.25.0

- **Breaking — raw external tokens are refused by default.**
  `auth.allow_raw_external_tokens` defaults to `false`, so a service account
  must exchange its external OIDC token at `/api/v1/auth/token` before calling
  project-scoped endpoints. The legacy in-handler verification path is then
  removed. **Prepare now:** aggregate the `rise::deprecation`
  `raw_external_token` events to find the workload identities that still
  present raw tokens, and move their CI to pre-exchange (see
  [Authentication & Tokens](/operator-docs/authentication/#access-hs256--token-exchange)).
  Tracked in [#374](https://github.com/rise-deploy/rise/issues/374).

## Not yet scheduled

- **Breaking — organization linkage becomes mandatory.** The
  `organization_resource_uid` columns on teams and projects become `NOT NULL`
  once startup backfill is proven reliable. Backend startup already backfills
  them, so no action is expected; the release that tightens them will say if
  that changes.
  Tracked in [#372](https://github.com/rise-deploy/rise/issues/372).
- **Action required (for controller authors) — organization opt-in for
  controllers.** Controllers that register `ResourceDefinition`s in an
  organization's own API group will need an explicit, operator-granted
  enablement step per organization. The design is still open. Tracked in
  [#437](https://github.com/rise-deploy/rise/issues/437).
- **Resource API — list pagination and label/field selectors.** `list` and
  `list_versions` return every row today; pagination and selectors land before
  high-cardinality kinds move onto the resource store. Additive. Tracked in
  [#368](https://github.com/rise-deploy/rise/issues/368).
- **Resource API — watch / change feed.** Lets controllers reconcile without
  polling `list`. Additive. Tracked in
  [#369](https://github.com/rise-deploy/rise/issues/369).
- **Resource API — secret fields.** Write-once, never-read-back values in the
  resource store. Additive. Tracked in
  [#370](https://github.com/rise-deploy/rise/issues/370).
- **Docker — no outage when a single-replica container is recreated.** The
  replacement container starts before the old one is removed, closing the
  short gap a drift-driven recreate leaves today. No action needed. Tracked in
  [#379](https://github.com/rise-deploy/rise/issues/379).
