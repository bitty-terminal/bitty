---
title: UX-37 semantic workspaces decision
description: Deferred decision record for semantic workspaces grouped by project cwd task (CTX-0679, bitty#1044)
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# UX-37 semantic workspaces decision

> Status: **draft decision proposal, deferred** (CTX-0679). This record
> proposes nothing normative and closes nothing: `OQ-052` is still
> owner-pending, so semantic grouping stays undecided.
>
> Task: `CTX-0679` | Issue: `bitty`#1044 | Backlog: `UX-37`
> Source: gap analysis window forms; `OQ-052` | Blocked: `OQ-052`

## Decision

**Defer** (Candidate): no semantic workspace grouping is adopted.
Named/grouped workspaces by project, cwd, or task, reusing
Workspace/View identity plus OSC 7, remain undecided until the owner
resolves `OQ-052`.

## Rationale

The issue is explicitly blocked on an owner decision. Adopting a
grouping key (project vs cwd vs task), an OSC 7 trust rule, or a
Workspace/View identity reuse scheme without the owner would
pre-empt the RFC. A deferred proposal preserves the option without
freezing a contract.

## Current state (no new contract)

- Structural workspace identity exists (`WorkspaceSceneId`,
  `WorkspaceSnapshot`, `WorkspaceId` mirrors) but carries no
  project/cwd/task semantics; nothing here groups by OSC 7.
- No code in this task adds grouping, naming, or OSC 7 handling.

## Gates to adopt later

- Owner resolves `OQ-052` (grouping key, identity reuse, OSC 7 trust).
- A follow-up RFC spells the grouping rule, the rename/merge
  behavior, and the fallback when OSC 7 is absent or untrusted.
- Implementation with bounded, deterministic grouping plus tests
  lands in `bitty-ui` behind that RFC.

## Open points

- Grouping key priority when project, cwd, and task disagree.
- OSC 7 spoofing threat model and the untrusted-input rule.
- Migration of existing structural workspaces to semantic names.
