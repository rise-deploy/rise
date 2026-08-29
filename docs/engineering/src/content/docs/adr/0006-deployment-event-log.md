---
title: "ADR-0006: Deployment Event Log"
---

## Status

**Draft** — no implementation. Date: 2026-08-30.

Draft rather than Proposed because [Open questions](#open-questions) 1 and 2
still reach into the Decision: the identity tuple observation events dedupe on
is not yet pinned per backend, and the ECS gaps in
[parity](#deployment-backend-parity) may force a different observation input
than the one D5 assumes.

## Context

Three surfaces want to answer "what happened to this deployment, and when": the
Timeline tab; a lifecycle rail on the deployment log console, which draws
deployment events on the log-volume time axis so a burst of errors can be read
against a replica coming up; and operators debugging a bad rollout, who today
leave Rise for `kubectl`, `docker` or the ECS console.

The rail is in flight rather than shipped, and it is what prompted this ADR: it
can only be built from the sources below, and doing so shows what they cannot
express. Where this document says the rail "reads events", it is describing the
consumer that motivates the design, not one that exists on `develop`.

Rise persists two things they could be built from, and neither is a history.

**The deployment row** carries `status`, `created_at`, `deploying_started_at`,
`first_healthy_at`, `completed_at`, `termination_reason` and `error_message`.
That is a usable skeleton of the happy path, and two of those columns
(`deploying_started_at`, `first_healthy_at`) are not serialized into the API
response at all — `src/server/deployment/handlers.rs` omits them. But it holds
one value per column: a deployment that went `Healthy → Unhealthy → Healthy`
looks exactly like one that never wavered.

**`controller_metadata.pod_status`** holds observed state. Every backend
produces it: Kubernetes builds its own in `src/server/deployment/webhook.rs`,
Docker and ECS through the shared builder in
`crates/rise-backend-core/src/pod_status.rs`, which produces the Kubernetes
shape the Pods tab consumes. It carries replica counts, per-container phase,
`restart_count`, and `state` with `started_at`, `finished_at` and `exit_code`.

The snapshot is a **level, not an edge** — what is true at the last observation,
which is a different thing from what happened:

- `restart_count` says a container has restarted eleven times. Nothing says when
  any of them happened, and the counter alone cannot distinguish eleven restarts
  from one restart observed eleven times.
- `last_state` holds exactly one prior termination, and only the Kubernetes path
  emits it — the shared builder has no notion of it. A crash-looping container
  therefore yields one marker on two of three backends, and none on the third.
- The snapshot is rewritten wholesale each tick, so nothing between two ticks is
  recoverable afterwards.

That last point is the load-bearing one, and it is what distinguishes this
decision from cheaper fixes. Serializing `deploying_started_at` and
`first_healthy_at` would repair the Timeline tab's happy path — see
[Alternatives](#alternatives-considered) — but no projection of a snapshot can
recover a termination that has already been overwritten. **The value of a log is
that it is written at tick rate; the value cannot be recovered at read time.**

## Decision

### D1. An append-only event log, alongside the snapshot

Rise persists `deployment_events`: an ordered, append-only record of things that
happened to a deployment. `controller_metadata.pod_status` stays exactly as it
is and keeps serving the Pods tab — it is the right shape for "what is true now"
and the wrong shape for "what happened".

### D2. One table, owned by `rise-deploy`

```sql
CREATE TABLE deployment_events (
    id            BIGSERIAL   PRIMARY KEY,
    deployment_id UUID        NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    occurred_at   TIMESTAMPTZ NOT NULL,
    recorded_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    kind          TEXT        NOT NULL,
    severity      TEXT        NOT NULL,
    source        TEXT        NOT NULL,
    message       TEXT,
    attributes    JSONB       NOT NULL DEFAULT '{}'::jsonb,
    dedupe_key    TEXT,

    CONSTRAINT deployment_events_severity_known
        CHECK (severity IN ('info', 'warning', 'error')),
    CONSTRAINT deployment_events_message_bounded
        CHECK (message IS NULL OR length(message) <= 4096),
    CONSTRAINT deployment_events_attributes_bounded
        CHECK (pg_column_size(attributes) <= 16384)
);

-- Reads (D7). The per-(deployment, kind) cap (D6) rides the same index and
-- filters `kind` in the scan: it walks one deployment, whose event count the cap
-- itself bounds, so a third index on the write path is not worth its cost.
CREATE INDEX deployment_events_by_recorded
    ON deployment_events (deployment_id, recorded_at DESC, id DESC);
-- The global age sweep (D6); `recorded_at` leads because the sweep is not
-- scoped to a deployment.
CREATE INDEX deployment_events_sweep ON deployment_events (recorded_at);
CREATE UNIQUE INDEX deployment_events_dedupe
    ON deployment_events (deployment_id, kind, dedupe_key)
    WHERE dedupe_key IS NOT NULL;
```

There is deliberately **no** `CHECK` on `kind`. A value list in SQL would be a
second copy of the vocabulary that D3 puts in `rise-backend-core`, needing a
migration per kind — and `deployments.status`'s own value-list `CHECK` has
already been rewritten by four separate migrations, which is the cost being
avoided. `severity` is constrained because it is three values that will not
grow. `attributes` is bounded because D3 passes runtime-native strings through
verbatim, and those name image references, ARNs and SSM parameter paths.

Migrations live in `rise-deploy`'s `migrations/`; every query goes through a
helper in `src/db/` per the crate's SQLX rule.

`BIGSERIAL` is a deliberate departure — every other primary key in this schema
is a UUID. Those identify addressable objects; this identifies a position in a
stream, and a monotone integer is what makes an incremental reader cheap. It is
**not** a commit order: values are assigned at tuple formation, so an event
inside a slow transaction can carry a lower id than one that commits before it.
D7 says what readers may rely on.

`occurred_at` is when the thing happened; `recorded_at` is when Rise found out.
The gap is real — a container that died at 12:04 and is noticed at 12:05 has
both, and the rail must plot the former. `occurred_at` comes from a foreign
clock (a `dockerd` host, a kubelet, AWS).

The writer therefore supplies both values explicitly rather than leaning on the
column default, and caps `occurred_at` at the `recorded_at` it is writing:
a `DEFAULT clock_timestamp()` cannot be referenced from the same statement's
value list, and a Rust-side comparison against a *different* host's clock would
let `occurred_at > recorded_at` through anyway. When the cap fires, the raw
value goes in `attributes.clock_skew`.

There is no lower clamp. Pinning early events up to `created_at` would collapse
the startup burst — several genuinely ordered replica events landing on one
timestamp — which is exactly the window the rail exists to show. An event dated
before its deployment is a signal worth seeing, and `created_at` can itself be
the wrong end of the skew.

### D3. One status kind, a few observation kinds, an open `attributes`

| `kind` | Meaning | Emitted by |
|---|---|---|
| `status_changed` | `attributes.{from, to, reason}` | status write (D4) |
| `replica_started` | A container began running | observation (D5) |
| `replica_terminated` | A run ended; exit code and reason in `attributes` | observation |
| `replica_restarted` | A container's restart counter advanced | observation |
| `scaled` | Desired replica count changed | observation |
| `backend_event` | A runtime-native event passed through | observation |

Status transitions are **one** kind carrying `from`/`to`, not one kind per
transition. A kind-per-transition vocabulary has to be kept isomorphic to
`DeploymentStatus` by hand, and it would be the *third* place the state machine
is mirrored — after `state_machine.rs` and the SQL `is_valid_transition`, whose
drift risk is already managed by an exhaustive test. One kind makes emission
total over the state machine by construction, and consumers switch on
`attributes.to`, which is the enum they already know.

`severity` is `info`/`warning`/`error` so consumers filter without a per-kind
table. `source` names the emitting backend (`kubernetes`, `docker`, `ecs`,
`control-plane`), which makes a parity gap legible in the data.

`backend_event` is the escape hatch for runtime-native facts: Kubernetes pod
events, ECS task stopped reasons, Docker daemon events. `attributes` carries the
native reason verbatim, bounded by the `message` limit in D2 — these strings can
name image references, ARNs and SSM parameter paths, and the row cap in D6
bounds rows, not bytes.

### D4. Status events are written where the status is written

Every guarded status `UPDATE` in `src/db/deployments.rs` emits one
`status_changed` for each row it changes. That is the ten `mark_*` writers
**and** `update_status`, which is a second writer owning the whole build path —
`Pending → Building → Pushing → Pushed` from the CLI's `PATCH .../status`, and
`Pushed → Deploying` from `lifecycle.rs`. `rollout_started` therefore comes from
`update_status`, not from any `mark_*`; an emission rule phrased over `mark_*`
alone would miss the most important status event.

Two writers need tightening first, and this is a prerequisite of the delivery
step rather than a follow-up:

- `mark_healthy_and_supersede` changes **two** rows in one call, and its second
  `UPDATE` is gated on `status = 'Healthy'` in a subquery rather than on
  `is_valid_transition`. Emission is per affected row, so both rows get an
  event, and that second statement should use the shared guard like every other.
- `update_status` performs a `SELECT`, a Rust-side `validate_transition`, and
  then the guarded `UPDATE`. The read is redundant with the guard and should go.

**The contract is at-least-once, not exactly-once.** `is_valid_transition`
returns true for `from == to` — deliberately, so `updated_at` can be refreshed —
so a duplicate write is a *legal* transition, not a rejected one. Two concurrent
metacontroller syncs both observing an unhealthy deployment will both succeed,
one on `Healthy → Unhealthy` and one on `Unhealthy → Unhealthy`, and produce two
events. A caller retry after a lost response does the same. And a genuine
`Healthy → Unhealthy → Healthy` produces two correct `status_changed` rows, so
no natural key can distinguish the duplicate from the repeat.

Consumers must therefore tolerate a repeated `status_changed` with the same
`from`/`to`. The rail already collapses coincident markers; the Timeline tab
should collapse adjacent identical transitions on display.

The insert is not free, either. Most writers are a single statement against a
`&PgPool` with no open transaction to join, so keeping the event atomic with the
status change means a data-modifying CTE — and the shape matters, because every
caller consumes the returned row:

```sql
WITH upd AS (UPDATE deployments SET … WHERE … RETURNING …),
     ev  AS (INSERT INTO deployment_events (…) SELECT … FROM upd)
SELECT * FROM upd;
```

The `INSERT` cannot be the outer statement: it returns nothing, and
`update_status` bails on `None` while `mark_healthy_and_supersede` derives its
outcome from the row. An event exists only if the row actually changed.

`mark_healthy_and_supersede` is the exception and needs no CTE at all — it
already opens a transaction and commits two `UPDATE`s, so two plain inserts join
it directly. That is a rewrite of each writer either way, not an added line.

### D5. Observation events are edges, derived against the previous snapshot

The reconcile tick already builds a fresh `pod_status`, and the previous one is
in hand on the `Deployment` row it is reconciling. Observation events are the
**difference between those two**, emitted before the new snapshot overwrites the
old.

There is prior art for exactly this diff, and it is not where one might expect:
the Kubernetes webhook already compares the previous `controller_metadata`
against freshly observed pods to carry terminated pods forward. The ECS
reconciler reads the snapshot back for its task-definition hash. The Docker
reconciler only writes it, so Docker is the one backend where this adds a
read — worth stating plainly, since delivery step 3 starts there.

This is a real comparison against remembered state. The snapshot is the memory,
and no new bookkeeping column is introduced — but the design is not
"stateless idempotency", and describing it that way would be wrong. Deriving
from the current observation alone can only ever emit facts still visible in it,
which reproduces exactly the sampling failure this ADR indicts: a container that
terminated and was replaced between two ticks never appears. Diffing against the
prior snapshot is what turns a level into an edge, and `restart_count` is the
clearest case — `7` this tick against `3` last tick is four restarts, where `7`
alone is only a number.

`dedupe_key` is a **concurrency guard, not the mechanism**. The Docker and ECS
reconcilers run under `rise-runtime-sync` leader election, so their ticks are a
single writer; the Kubernetes webhook is called by metacontroller and is not
leader-gated, so two syncs can derive the same edge concurrently. The partial
unique index makes that collision harmless. Because it is a partial index, the
insert must spell the predicate —
`ON CONFLICT (deployment_id, kind, dedupe_key) WHERE dedupe_key IS NOT NULL DO NOTHING`.
Postgres cannot infer a partial unique index otherwise and raises
"there is no unique or exclusion constraint matching the ON CONFLICT
specification" immediately, so this fails loudly in the first test rather than
degrading quietly.

Identity is a backend-supplied tuple, `(source, replica_id, container_name)`,
because "container identity" is not one thing: Docker has a generation-ful
container name that changes on recreate, ECS has a task id from an ARN that
changes on task replacement, Kubernetes needs the pod name *and* the container
name to be unique. Each backend produces the tuple explicitly rather than the
derivation guessing.

Timestamps are normalized before they enter a key. Kubernetes serializes
`startedAt`/`finishedAt` as RFC3339 at **second** granularity, Docker gives
nanoseconds, ECS gives its own rendering — so keys are built from a parsed
timestamp truncated to seconds, plus a discriminator that does not depend on
clock resolution (the restart ordinal for `replica_restarted`, the prior
snapshot's state for the others). Keying on a raw vendor string would make
dedupe correctness depend on a daemon's formatting. A field that can be absent —
`started_at` is null for a container that never ran — never appears alone in a
key; the tuple falls back to the identity plus the transition it represents.

`scaled` carries **no** `dedupe_key`. It is emitted only when the desired count
in the new snapshot differs from the old, and that comparison is the whole
guard. A key containing an observation timestamp would never collide and would
emit every tick; a key of `(previous_count, new_count)` would be worse, because
a deployment oscillating 1→3→1→3 would record the first two changes and then go
permanently silent — the same "eleven restarts look like one" failure this ADR
exists to fix. `scaled` therefore behaves like `status_changed`: at-least-once,
de-duplicated by nothing, with consumers tolerating a repeat.

### D6. Retention: swept by `recorded_at`, on its own cadence

An age sweep (proposed: 30 days, and see [Open question 4](#open-questions))
bounds the table, and a per-`(deployment_id, kind)` cap bounds a single crash
loop. The sweep runs on its own `GlobalSchedule` at an hourly cadence, following
`ProjectController`'s expired-state cleanup, which is the same primitive. It
does **not** join the resource-GC loop: that ticks every 10 seconds and is built
for tombstoned resource rows with batch sizes and per-row leader re-checks, none
of which fits a bulk time-range delete.

The sweep keys on `recorded_at`, not `occurred_at`. `occurred_at` is a clamped
foreign clock and is what display sorts by; retention must be driven by when
Rise wrote the row.

The cap is per `(deployment_id, kind)` rather than per deployment because a
global oldest-first cap deletes the wrong end. A crash loop is the highest-rate
producer, so it is what hits any cap — and evicting oldest-first would delete
the `status_changed` rows and the first `replica_terminated` carrying the real
exit code, keeping a thousand near-identical restarts. The anchors of an
incident are the useful part.

Because D5 derives from the snapshot rather than from the event rows, deleting
an event cannot resurrect it. This is the direct payoff of not making
`dedupe_key` the mechanism: an idempotency scheme whose memory is the row itself
un-dedupes the moment retention removes that row, and would re-emit a
still-running container's `replica_started` on every sweep interval forever.

Retention is deliberately shorter than a long-lived deployment's lifetime.
Events are a debugging aid, not an audit log, and nothing about correctness may
come to depend on an old event still being present.

### D7. Read API, and what a reader may rely on

```
GET /api/v1/projects/{project}/deployments/{deployment_id}/events
      ?limit=&cursor=&kind=&severity=
```

Authorization follows the sibling log endpoint exactly: resolve the project by
name, `resolve_for_project` (which admits project-scoped service accounts and
controllers), then `ensure_project_access_or_admin` for human callers, with the
resolve step's 401/403 remapped to **404** so project existence does not leak —
the membership check that follows returns its own 403 — and the deployment
looked up scoped by `project.id`. There is no write path in this
API — see D11.

**Ordering, precisely.** Display sorts by `(occurred_at DESC, id DESC)`.
Pagination and incremental polling key on `(recorded_at, id)`, because
`occurred_at` can move backwards: an event derived from a late observation can
carry an older `occurred_at` than one already returned, and a keyset cursor over
`occurred_at` would step past it permanently. `recorded_at` is monotone per
writer but is not a commit timestamp either, so an incremental reader overlaps
its window by a small margin and de-duplicates on `id`. A single page is
internally consistent; the feed as a whole is eventually consistent, and that is
stated rather than implied.

The cursor reuses the log endpoint's base64 encoding and its digest helper. It
is worth being accurate about what that buys: `log_cursor_signature` is an
**unkeyed** SHA-256 over public inputs, so it detects a cursor reused against a
different query — it does not prevent a forged one. `deployment_id` is therefore
always taken from the path, never from the cursor, so a forged cursor cannot
page across deployments. The digest must cover `kind` and `severity`, which the
log endpoint's inputs do not include. Note also that the log endpoint **rejects**
a cursor combined with a time range; this endpoint has no `since` parameter for
the same reason.

Plain JSON, not SSE. Events are low-rate, the console already polls the
deployment for status, and a second streaming transport is not justified until
something needs sub-second delivery.

### D8. Consumers

The lifecycle rail reads events instead of deriving markers from `pod_status`.
The Timeline tab renders the log directly, and the client-side timeline
synthesis is deleted. The Pods tab is untouched — it wants current state.

### D9. Events are not log lines

Deployment events are control-plane facts about a workload; application logs are
output from inside it. Writing events into the log backend would give them that
backend's retention, query language and outages, and would put them behind a
Loki or CloudWatch an operator may not have configured — the default Kubernetes
log backend has no historical store at all. They would also stop being queryable
by kind.

The useful coupling is the one D7 preserves: both feeds are timestamped and
paginated over the same window, so the console can lay one over the other
without either owning the other.

### D10. Parity is explicit, and asymmetry is data

The floor every backend meets is `status_changed`, which comes from the shared
status writers and is runtime-independent. Observation events are where backends
genuinely differ, and `source` records which produced each one, so a consumer
can say "this backend does not report that" from the data.

### D11. This decision is scoped to in-process controllers

D4 and D5 both assume the emitting code holds a `PgPool`: the status insert is a
CTE on the same statement, and the observation derivation reads
`controller_metadata` directly. ADR-0004 places deployment controllers **outside**
the control-plane process boundary. If that lands, the status half survives
unchanged — those writes are control-plane-local — and the observation half
needs an authenticated event-write endpoint, at which point `source` becomes
caller-asserted rather than the trustworthy field D10 relies on, and needs
binding to the controller's identity.

This ADR does not design that endpoint. It records that the observation path is
the part that must be revisited when ADR-0004 proceeds, so the coupling is a
known cost rather than a surprise.

## Deployment-backend parity

Intended v1 reach. ⚠️ is a tracked gap; ❌ is a limitation of the runtime.

| Event | Kubernetes | Docker | ECS |
|---|---|---|---|
| `status_changed` | ✅ | ✅ | ✅ |
| `replica_started` | ✅ | ✅ | ✅ |
| `replica_terminated` | ✅ | ✅ | ⚠️ rolling replacements invisible |
| `replica_restarted` | ✅ | ✅ | ❌ no in-place restart |
| `scaled` | ✅ | ✅ | ✅ |
| `backend_event` | ✅ pod events | ⚠️ daemon event stream unused | ✅ task stopped reasons |

`status_changed` is uniform by construction.

**ECS `replica_restarted` is ❌, not a gap.** `restart_count` is set to `None`
unconditionally in the ECS reconciler because Fargate does not restart a
container in place — a stopped task is replaced by a new task with a new ARN.
There is no counter to advance. The ECS analogue of a restart is a
`replica_terminated` / `replica_started` pair, which is what the rail should
render there.

**ECS `replica_terminated` is ⚠️.** The reconciler partitions observed tasks by
`task_definition_arn`, so tasks from an outgoing revision never enter the
observation set — meaning terminations during a rolling replacement, the most
interesting moment of a bad rollout, are not seen. Closing this widens the
observation input, which touches the readiness path rather than riding on it.

**Docker `backend_event` is ⚠️, not a limitation.** bollard exposes a daemon
event stream carrying `die`, `oom`, `health_status` and `restart`; the backend
does not consume it. The absence of a scheduler rules out `FailedScheduling`-class
events, not the category.

The matrix in [Deployment Backends](../deployment-backends.md) gains a row in
the PR that first makes events readable — delivery step 5 — not in this ADR. The
matrix documents what ships.

## Consequences

**Good.** History becomes queryable in one shape across all three backends. The
Timeline tab stops inventing content. The rail can show a crash loop rather than
one arbitrary termination. Parity gaps become visible in the data via `source`.

**Costs.** A new table and migration on the control plane's primary database,
with three indexes on the write path and no partitioning story yet. Two status
writers must be tightened before step 2 (D4). The observation derivation needs
the Kubernetes path normalized onto `InspectedContainer` first: it builds its
`pod_status` JSON directly from `k8s ContainerStatus` and its shape already
diverges from the shared builder's, so "all three backends share the derivation"
is work, not a free ride. Retention becomes a policy someone will want to
configure per install.

**Operator impact: not None.** A migration, a new table with a retention default
that silently deletes data, and a new API endpoint. This needs an
[Upgrade Notes](../upgrade-notes.md) entry in the release it lands in, and the
workstream is tracked on the Rise Rollout Tracker. The per-install retention
setting (Open question 4) should be filed as a `rollout-gate` issue when the
first slice lands.

**Explicitly not solved.** Build-pipeline events beyond what the CLI already
reports — and what it reports is best-effort, because the CLI deliberately
swallows status-update failures, and an `--image` deploy skips `Building`
entirely. Cross-deployment or project-level history. Anything audit-grade.
A `rise deployment events` CLI surface, which should follow but is not designed
here.

## Alternatives considered

**Serialize the columns that already exist.** Add `deploying_started_at` and
`first_healthy_at` to the deployment response and let the Timeline tab render
the real timestamps. Zero schema change, and it genuinely fixes the invented
pipeline. Rejected as *insufficient*, not wrong — it should probably be done
regardless, and it is the reason this ADR does not lean on the Timeline tab as
its justification. It cannot express anything that happened more than once, and
it says nothing about replicas.

**A status-history table only.** `(deployment_id, from, to, at, reason)`, written
in the same guarded `UPDATE`. Delivers the Timeline tab completely, with no
`kind` vocabulary, no `attributes`, no dedupe, no retention policy and no
observation derivation. A genuinely smaller design, and if the observation half
proves too costly this is the fallback. Rejected because the replica-level
events are the content the rail exists for, and adding them later means either a
second table or widening this one into the general shape anyway.

**Extend `controller_metadata` to carry history.** A ring of recent
terminations inside the existing JSON. Rejected: it is rewritten wholesale each
tick, so it holds only what the writer carried forward; it grows the deployment
row on the hot path; and it cannot be queried by kind or time without loading
the blob.

**Emit into the log backend.** Rejected in D9.

**Emit as OpenTelemetry events or spans**, letting the operator's observability
stack own storage and retention. Attractive — it is someone else's retention
problem, and correlates with traces. Rejected for v1 because Rise's own UI is
the primary consumer and cannot depend on an optional external collector being
configured; the Timeline tab must work on a default install. Worth revisiting as
an *additional* export once the internal shape is settled.

**Reuse the generic resource API.** Model events as a resource kind and inherit
storage, authorization and GC. Rejected for v1: that API authorizes per object
through the ADR-0001 engine, which is the right cost for user-managed objects
and the wrong one for a machine-written stream. Deployments are a typed API with
their own authorization path and their events should follow the parent. Worth
revisiting if the typed-object migration brings deployments under that API, at
which point this table is an implementation detail behind the same endpoint.

**Derive events at read time by diffing stored snapshots.** Rejected: it needs
snapshot history, and it inherits the sampling problem completely — anything
between two ticks stays invisible, and the read path pays for the diff every
time. Note this is *not* an argument against write-time diffing, which D5 does
against the one prior snapshot that already exists.

**Adopt the Kubernetes `Event` shape.** Rejected on aggregation semantics rather
than field names: its `count`/`lastTimestamp` mutate in place, losing the
individual occurrences that are the whole point here. (`events.k8s.io/v1` does
separate a series from individual events, so this is a rejection of the classic
shape, not of everything Kubernetes offers.)

## Delivery outline

1. **Table, model, read API.** Migration, `src/db/deployment_events.rs`, the
   `kind` vocabulary in `rise-backend-core`, `GET .../events` with the D7
   authorization chain. Nothing emits yet.
2. **Status events.** Tighten `update_status` and `mark_healthy_and_supersede`
   per D4, then emit `status_changed` from every guarded status `UPDATE`.
   Uniform across backends immediately.
3. **Observation events, Docker first.** The snapshot diff in
   `rise-backend-core`, wired into the Docker reconciler, where the identity
   tuple and the restart counter are both well-defined. This is the slice that
   validates D5 before it is generalized.
4. **ECS and Kubernetes observation.** Normalize the Kubernetes path onto
   `InspectedContainer`; decide the ECS observation input per Open question 2.
5. **Consumers and the parity matrix.** Rail and Timeline tab read events; the
   client-side timeline synthesis is deleted; the feature matrix gains its row.
6. **Retention.** Age sweep and per-kind cap on their own schedule.
7. **`backend_event` pass-through.** Kubernetes pod events, ECS stopped reasons,
   Docker's daemon stream.

Steps 1–2 are independently useful. Step 3 takes Docker first for its identity
model, not for familiarity with the diff: Docker has a stable per-replica
container name and a real restart counter, where ECS has neither. It is also the
only backend that does not already read the previous snapshot back, so the cost
of doing so lands where the rest of the design is simplest. The Kubernetes path
already performs an equivalent diff, which makes step 4 a normalization job
rather than a new mechanism.

## Testing

- Every legal status transition emits one `status_changed` with the expected
  `from`/`to`, alongside the existing
  `db_is_valid_transition_matches_rust_is_valid_transition`. Self-transitions
  are included and assert the at-least-once contract explicitly rather than
  being treated as impossible.
- Pure tests for the snapshot diff: given two consecutive `pod_status` values,
  assert the events. Same style as the existing `pod_status` builder tests, no
  daemon. Cases that must appear: a restart counter jumping by more than one, a
  container replaced by one with the same name, an absent `started_at`, and two
  replicas starting within the same second.
- Concurrency: two derivations of the same edge insert once, asserted against
  the partial index with the predicate spelled.
- Clamping: an `occurred_at` before `created_at` and one in the future both land
  in range with `attributes.clock_skew` set.
- Read API: keyset pagination over `(recorded_at, id)` returns every row exactly
  once under concurrent appends, including an append with an older
  `occurred_at`.
- e2e: a deployment through to `Healthy` produces the expected ordered kinds on
  each backend the harness runs.

## Open questions

1. **The identity tuple, per backend.** D5 specifies
   `(source, replica_id, container_name)`. What fills `replica_id` on ECS when a
   slot has no running task is unresolved — the reconciler substitutes a
   synthetic label there — and identity is what the snapshot diff matches
   containers on between ticks, so a slot whose identity alternates between a
   task id and a label reads as a container disappearing and a new one
   appearing. That is a correctness question for the *diff*, not for dedupe:
   ECS is leader-gated, so `dedupe_key` does no work there. Blocks step 4.

2. **The ECS observation input.** Closing the ⚠️ on `replica_terminated` means
   observing tasks outside the target task-definition revision, which changes
   what the readiness path looks at. Whether that is worth coupling to is a
   design question this ADR does not settle.

3. **Write cost.** D5 only attempts an insert when the diff is non-empty, so
   steady state should write nothing — but the diff itself runs every tick, and
   the claim should be measured in statements and index probes, not rows, before
   step 3 ships.

4. **Per-install retention configuration.** D6 proposes fixed defaults. Whether
   this becomes a settings key, and whether it belongs beside `deployment_logs`
   or in its own block, is deferred.

5. **Partitioning.** An append-only table on the primary database with three
   indexes will eventually want a partitioning or vacuum story. Not needed for
   v1 volumes; should not be discovered at the wrong moment.

## References

- ADR-0001 — the authorization engine whose per-object cost the generic-resource
  alternative was weighed against.
- ADR-0004 — Control-Plane Process Topology; D11 records the coupling.
- ADR-0005 — ECS Deployment Backend, for the parity conventions this follows.
