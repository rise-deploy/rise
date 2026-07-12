---
title: "ADR-0002: Generic Resource Subresource Execution Model"
---

## Status

**Draft.** Date: 2026-07-12.

This is an exploratory design, not yet a proposed decision. It may change
substantially before promotion to **Proposed**. ADR-0001 fixes the authorization
shape `(verb, kind, subresource?)` and the initial `status`, `finalizers`, and
`token` semantics; this draft explores how authorized subresources execute.

## Context

The generic resource API currently has special routes for `status` and
`finalizers`. ADR-0001 replaces their controller allowlist with unified RBAC,
models token issuance as `create` on `/token`, and reserves the same
authorization shape for future endpoints such as `/logs`, `/scale`, and
`/proxy`.

Those operations do not all behave like ordinary stored-resource CRUD:

- `status` and `finalizers` mutate protected portions of the parent object;
- `scale` would expose a virtual projection of a parent resource;
- `token` creates and returns a credential without storing a Token resource;
- `logs` would stream backend data that is not present in the resource store;
- `exec` or `proxy` could establish a long-lived connection to another system.

If each product handler owns routing, authorization, parent lookup, timeouts,
auditing, and error mapping, the platform recreates the fragmented security
model ADR-0001 is intended to remove. Conversely, forcing all operations
through the stored-resource CRUD interface would make streaming and proxying
awkward or unsafe. The generic layer needs one execution seam broad enough for
these shapes without becoming an untyped escape hatch around RBAC.

## Draft decision

### 1. A subresource is registered against a parent kind

A `ResourceDefinition` may declare named subresources. Each declaration names
a platform-known handler strategy and the operations that strategy exposes:

```yaml
subresources:
  - name: status
    handler: status
    verbs: [get, update]
  - name: logs
    handler: deployment-logs
    verbs: [get]
```

The exact serialized schema remains open. The intended invariants are not:

- names use the canonical lowercase subresource grammar from ADR-0001;
- the handler identifier must exist in the process's code-backed registry;
- declared verbs must be a subset of that handler's supported verbs;
- duplicate names and ambiguous registrations are rejected;
- an undeclared `(kind, subresource)` route does not exist;
- registration never grants access — RBAC is evaluated separately per request.

This is not a plugin ABI. A `ResourceDefinition` selects from handlers already
compiled and wired into the platform; it cannot name a URL, executable, dynamic
library, or arbitrary Rust type. Generic strategies such as `status` may be
enabled by custom kinds. Product handlers such as `deployment-logs` are
registered only for compatible built-in kinds unless a later ADR deliberately
opens that boundary.

### 2. One shared request pipeline surrounds every handler

The generic resource API owns this sequence:

```text
route and validate the registered subresource
→ authenticate to a typed Rise principal
→ resolve the parent resource and authorization context
→ authorize (verb, kind, subresource) through ADR-0001
→ apply concurrency, admission, and request-limit policy
→ invoke the registered handler
→ map its typed result to HTTP
→ complete the audit record
```

The handler is never passed a JWT or raw `sub`, never decides whether the
caller holds the primary subresource permission, and never chooses an
authorization verb from request data. It receives a typed context containing
the authenticated principal, already-resolved parent identity and object,
resource definition, canonical subresource, request deadline/cancellation, and
an audit correlation identifier.

Some handlers may need additional authorization decisions on resources they
reference. Those go back through the same authorization service as explicit
secondary checks; a handler cannot manufacture an `Allowed` result or accept a
caller-supplied subject. Whether any initial handler needs this remains open.

Parent lookup, scope and label resolution, denial/existence masking, and
tombstone behavior inherit ADR-0001 and the generic resource API contract. A
handler is not invoked when those checks fail.

### 3. Handler results are typed by execution shape

The execution seam must support at least these categories without giving every
handler an unrestricted raw HTTP response:

| Shape | Examples | Result |
|---|---|---|
| Protected stored-field mutation | `status`, `finalizers` | Updated parent resource |
| Virtual resource projection | `scale` | Typed projection, optionally mapped back into the parent |
| Generated finite response | `token` | Typed non-persisted response with explicit sensitivity metadata |
| Server stream | `logs` | Content type plus cancellable, backpressured byte stream |
| Upgraded or duplex connection | possible `exec` | Audited connection/session handle |
| Reverse proxy | possible `proxy` | Constrained upstream exchange |

The first four shapes are the immediate design target. Upgrades and reverse
proxying remain candidates, not commitments: their security and transport
requirements may justify separate interfaces or rejection altogether.

The final Rust trait and result enum remain open. Regardless of their shape,
handlers must declare supported methods, request/response content types,
whether they mutate the parent, and whether their response contains secrets.
The generic layer rejects a mismatched method or content type before invocation.

### 4. Stored-field strategies are generic

`status` and `finalizers` use shared strategies rather than per-kind handlers.
Their Kubernetes-style mutation behavior is normative in ADR-0001: main writes
preserve protected fields, subresource writes preserve everything else, and
the resulting whole object passes normal validation, optimistic concurrency,
persistence, and audit handling. Status-only writes do not increment
`metadata.generation`.

A kind may register an optional typed validator for legal status or finalizer
transitions. It does not reimplement routing, authorization, patch/apply field
filtering, resource-version checks, or storage.

The handling of server-side apply field ownership across these strategies must
be specified and tested before this ADR becomes **Proposed**.

### 5. Generated and virtual responses do not imply stored resources

`POST <ServiceAccount-or-Controller>/token` is dispatched as a generated
finite response. It may return a credential after trust-policy and one-hop
checks, but it creates no Token row and exposes no `get token` operation.

A future `/scale` may return a versioned scale representation projected from a
Deployment and map an authorized update back to a declared parent field. Its
schema, conversion, validation, and conflict behavior must be explicit in the
handler registration; it cannot patch arbitrary parent fields.

Generated and virtual responses are serialized from allowlisted response
types. They do not let handlers return an arbitrary stored-resource envelope
that could be mistaken for the parent.

### 6. Streams are bounded and auditable

A streaming handler must honor cancellation and backpressure and must have
platform-enforced connection, idle, and maximum-duration limits. Exact defaults
and whether operators may configure them per handler remain open.

Audit records are two-phase for long-lived operations: a start record after
authorization and a completion record containing outcome, duration, and byte
counts. Log contents, token bodies, authorization headers, and other response
payloads are never written to the audit log. Disconnect and timeout are
distinct completion outcomes rather than silent successful requests.

### 7. Discovery describes capability, not permission

The API should expose which subresources and verbs a served kind implements so
clients need not guess. Discovery describes the registered API surface; it
does not claim the current caller is authorized. The endpoint shape and whether
discovery itself requires authentication remain open.

### 8. Product semantics stay outside the generic core

The generic crates define registration, request context, result shapes,
security ordering, cancellation, and audit hooks. A Deployment log handler may
use backend-specific services supplied by `rise-deploy`, but the generic core
does not learn about Docker, Kubernetes, log retention, replicas, containers,
or deployment topology.

The following are explicitly not decided here:

- whether Rise will ship `/logs`, `/scale`, `/exec`, or `/proxy`;
- log source, retention, multiplexing, follow/tail query syntax, and formats;
- Deployment replica/container selection;
- proxy target selection and permitted protocols;
- terminal resize and attach/exec protocols;
- backend-specific availability and fallback behavior.

Each shipped product subresource may require its own smaller ADR or API design.

## Open questions before Proposed

- What is the exact `ResourceDefinition.subresources` schema?
- What are the Rust registration, request-context, and result interfaces?
- Are handler registrations static at process startup, and how are collisions
  diagnosed across crates?
- Which verbs and media types do the initial `status`, `finalizers`, and
  `token` strategies expose?
- How do apply field managers and managed fields behave on protected fields?
- Does UID addressing admit every registered subresource identically to named
  addressing?
- What are the common timeout and response-size defaults?
- Is discovery authenticated, and is it part of the existing resource
  discovery document or a new endpoint?
- Should upgraded connections and reverse proxying use this seam or a narrower
  follow-up abstraction?

## Consequences if adopted

**Positive.** Authorization, parent resolution, auditing, error mapping, and
request limits remain centralized. Stored, virtual, generated, and streaming
operations can share one API vocabulary without pretending they share one
storage model. New handlers cannot silently define their own auth scheme.

**Negative.** The generic API gains a more complex typed execution interface
and lifecycle. Streaming and generated responses expand the testing matrix
beyond JSON CRUD. A code-backed registry means arbitrary operator-defined
remote handlers are intentionally unsupported.

**Risk.** An interface generalized for hypothetical proxy or upgrade use may
become needlessly broad. Keeping those shapes non-committal until a concrete
use case exists limits that risk.

## Alternatives considered

- **One bespoke HTTP route per operation.** Simple locally, but duplicates
  authorization, parent lookup, masking, audit, and timeout behavior. Rejected
  as the default direction because it recreates fragmented enforcement.
- **Treat every subresource as an independently stored resource.** Fits CRUD
  but misrepresents streams, credentials, proxies, and projections, and creates
  synchronization problems for status. Rejected.
- **Give handlers an unrestricted raw HTTP request and response.** Maximally
  flexible but makes security, response limits, secret handling, and audit
  behavior unenforceable by the shared layer. Rejected in favor of typed
  contexts and result shapes.
- **Expose a runtime plugin or webhook system immediately.** Adds remote-code
  trust, availability, versioning, and credential-forwarding concerns before a
  concrete extension use case exists. Rejected for the initial model; handlers
  are code-backed and platform-known.
- **Finish only `status`, `finalizers`, and `token`, then design the seam when
  logs arrive.** Minimizes current design work but risks baking a finite-JSON
  assumption into `ResourceDefinition`. Rejected as the reason to keep this
  draft now, while leaving streaming details open.

## References

- [ADR-0001: Unified Permission Model](../0001-unified-permission-model/)
- [Generic Resource API](../../generic-resource-api/)
