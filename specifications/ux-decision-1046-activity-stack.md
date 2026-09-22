---
title: UX-39 U-2 ActivityStack decision
description: Decision record for U-2 ActivityStack push/pop navigation inside a Panel (CTX-0679, bitty#1046)
category: specifications
audience: contributor
document_type: design-record
status: accepted
---

<!-- markdownlint-disable MD025 -->

# UX-39 U-2 ActivityStack decision

> Status: **accepted decision record** (CTX-0679). This record adopts the
> shipped `ActivityStack` behavior as the U-2 answer and verifies it as
> done. It accepts no RFC amendment: Activity generalization beyond
> push/pop stays Open for the owning RFC.
>
> Task: `CTX-0679` | Issue: `bitty`#1046 | Backlog: `UX-39`
> Source: `panel-runtime-rfc.md` recorded direction | Depends: `UX-15`

## Claim-status legend

- **Shipped** — implemented and tested in the `bitty` repository.
- **Accepted** — decided in this record.
- **Open** — follow-up work with no contract.

## Decision

**Adopt** (**Accepted**): push/pop navigation inside one Panel is the
shipped `ActivityStack` in `crates/bitty-ui/src/workspace_scene.rs`:

- `ActivityStack::new(panel, entry)` starts with the entry activity.
- `push` appends, failing closed with `SceneError::ActivityOverflow`
  at `MAX_ACTIVITY_DEPTH` (32); the stack is unchanged on refusal.
- `pop` returns to the previous activity; popping the entry is a
  no-op success returning the entry (the stack never empties).
- `replace` swaps the current activity without changing depth.

## Rationale

The issue scope asks exactly for push/pop navigation inside a Panel.
The shipped type covers that scope with a bounded, fail-closed,
deterministic contract and no wall-clock, randomness, or platform
handle. Activity generalization (what an Activity is beyond an
`ActivityId`) is explicitly out of scope and stays Open.

## Evidence (**Shipped**)

- Implementation: `crates/bitty-ui/src/workspace_scene.rs`
  (`ActivityStack`, `ActivityId`, `MAX_ACTIVITY_DEPTH`,
  `SceneError::ActivityOverflow`).
- Unit tests in the same file: `activity_stack_push_pop_replace`,
  `activity_stack_overflow_fails_closed`.
- Integration coverage: `crates/bitty-ui/tests/u1_u2_model.rs`.
- Reproduce: `cargo test -p bitty-ui --test u1_u2_model`
  and `cargo test -p bitty-ui workspace_scene`.

## Gates

- Overflow stays fail-closed at the documented cap.
- Entry pop stays a no-op success; depth never reaches zero.
- `cargo fmt --check`, `cargo clippy --workspace --all-targets`
  with `-D warnings`, and `cargo test -p bitty-ui` stay green.

## Open points

- Activity generalization (lifecycle, focus routing composition,
  persistence) needs an RFC amendment; tracked by the owning RFC,
  not this record.
- User-visible identity of stacked activities (`ActivityId`
  display) is Open.
