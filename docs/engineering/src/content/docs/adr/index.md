---
title: "Architecture Decision Records"
sidebar:
  order: 0
---

ADRs record point-in-time architectural decisions: the context they were made
in, the alternatives that were rejected, and the consequences accepted. They
complement `ROADMAP.md`, which owns live workstream status and phase rationale
— an ADR records *why a decision was made*; the roadmap tracks *where the work
stands*.

## When to write one

Write an ADR for decisions that shape architecture across multiple PRs, are
expensive to reverse, or reject a plausible alternative someone will
re-propose later. Routine implementation choices don't need one.

## Format

One file per decision in this directory, named `NNNN-short-title.md` with a
monotonically increasing number. Sections: **Status**, **Context**,
**Decision**, **Consequences**, **Alternatives considered**, plus an optional
**References**.

Statuses: **Proposed**, **Accepted**, **Superseded by ADR-NNNN**. A superseded
ADR is never edited to say something else — it gets a status pointer to its
successor.

## Index

| ADR | Status | Summary |
|---|---|---|
| [ADR-0001](/operator-docs/adr/0001-unified-permission-model/) — Unified Permission Model | Proposed | One Role/RoleBinding/ceiling model for all subject kinds (Users, Teams, ServiceAccounts, Controllers, Operators) on the generic resource API and token issuance. |
