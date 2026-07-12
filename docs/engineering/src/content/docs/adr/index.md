---
title: "Architecture Decision Records"
sidebar:
  order: 0
---

ADRs record architectural decisions: the context they were made in, the
alternatives that were rejected, and the consequences accepted. An ADR's
**Status** field tracks the decision through its whole lifecycle, from
proposal to implementation.

## When to write one

Write an ADR for decisions that shape architecture across multiple PRs, are
expensive to reverse, or reject a plausible alternative someone will
re-propose later. Routine implementation choices don't need one.

## Format

One file per decision in this directory, named `NNNN-short-title.md` with a
monotonically increasing number. Sections: **Status**, **Context**,
**Decision**, **Consequences**, **Alternatives considered**, plus an optional
**References** and appendices where they earn their keep.

Statuses: **Draft**, **Proposed**, **Accepted**, **In Progress** (implementation
underway), **Implemented**, **Superseded by ADR-NNNN**. A superseded ADR is
never edited to say something else — it gets a status pointer to its
successor.

**Draft** is pre-decision working material: it records a design direction and
open questions but is not yet ready for review as a proposed architecture.

## Index

| ADR | Status | Summary |
|---|---|---|
| [ADR-0001](./0001-unified-permission-model/) — Unified Permission Model | Proposed | One Role/RoleBinding model for all subject kinds (Users, Teams, ServiceAccounts, Controllers, Operators) on the generic resource API and token issuance, plus the crate structure that realizes it. |
| [ADR-0002](./0002-generic-resource-subresource-execution-model/) — Generic Resource Subresource Execution Model | Draft | A typed registration and execution seam for stored, virtual, generated, streaming, and possible connection-oriented subresources. |
