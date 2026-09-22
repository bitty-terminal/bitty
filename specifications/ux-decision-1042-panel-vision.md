---
title: UX-35 U-9 panel-vision Vision v2 archive decision
description: Deferred decision record for retiring or archiving panel-vision.md (CTX-0679, bitty#1042)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# UX-35 U-9 panel-vision Vision v2 / archive decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> retires nothing and archives nothing: `panel-vision.md` is canonical
> content in `bitty-terminal-docs` and its fate is owner-pending.
>
> Task: `CTX-0679` | Issue: `bitty`#1042 | Backlog: `UX-35`
> Source: `U-9`; `panel-vision.md` | Depends: `UX-34`

## Decision

**Defer** (Candidate): keep `panel-vision.md` as-is. Neither a Vision
v2 rewrite nor an archive move is adopted in this task. The canonical
file stays owned by `bitty-terminal-docs`; this repository records
only the deferral and its unblocking conditions.

## Rationale

Retiring or archiving a canonical vision document is a docs-corpus
decision with cross-repo effects (links, roadmap references, U-9
scope). Doing it from the `bitty` side while `UX-34` and the owner
review are pending would orphan references and pre-empt the owner.
Deferral is the only honest option.

## Current state (no new contract)

- No `panel-vision.md` copy is added, moved, or rewritten here.
- U-9 implementation in `bitty-ui` stays at the shipped candidate
  modules; no Vision v2 API is introduced.

## Gates to adopt later

- `UX-34` lands and the owner approves either Vision v2 scope or the
  archive (including redirect and reference updates).
- The change executes in `bitty-terminal-docs` with this repository
  syncing only the pin and affected links.

## Open points

- Vision v2 scope versus archive-and-replace.
- Link and roadmap fallout of an archive move.
- U-9 implementation alignment after the docs decision.
