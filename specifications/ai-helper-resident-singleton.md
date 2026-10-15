---
title: Core-side Resident-Singleton Helper Spawn Contract (DIR-030 S-1)
description: Core-side spawn contract for the native `ai` helper (executable `bitty-ai`) as a resident stdio coprocess — resolution, digest verification, bitty-ipc framing, and bounded supervision
category: specifications
audience: contributor
document_type: design-record
status: draft
---

<!-- markdownlint-disable MD025 -->

# Core-side Resident-Singleton Helper Spawn Contract (DIR-030 S-1)

> Task: `CTX-1253` | Issue: `bitty`#2123 | Program: `AI-0302` (helper side)
> Direction: DIR-030 native components | Ruling: S-1 adopted 2026-10-15
> (`bitty-ai-docs/architecture/execution-ownership-r1.md`)
>
> This record documents the Core-side half of the adopted S-1
> (metadata-only terminal registry) resident-singleton helper: **resolution**,
> **digest verification**, **stdio-coprocess spawn** over the accepted
> bitty-ipc framing, and a **bounded supervision/restart policy**. It closes no
> open question and no AIQ entry; it records implemented Core behavior with
> test evidence from this repository.

## Scope

Core owns the process, the pipe pair, and the restart policy for the native
`ai` helper. The helper-side endpoint (program `AI-0302`, slices E0..E3) owns
the `ai.*` handler surface, the snapshot-version correlator, the per-request
gates, and all provider/model I/O. Core never links AI code, never loads it
in-process, and never holds a credential.

The implemented Core seam is `bitty_runtime::ai_helper` (module
`crates/bitty-runtime/src/ai_helper.rs`), driven through `ResidentHelper`.

## 1. Resolution

- The helper is the native component `ai` under the DIR-030 install root. Core
  reuses `crate::component::resolve_search`, i.e. the same user-over-system
  two-tier search as `net`: user `<data_home>/bitty/extensions` wins on
  collision, the system tier (`/usr/lib/bitty/extensions` or platform
  equivalent) is read-only, and a tampered user install fails closed without
  system fallback.
- **`PATH` is never read.** The executable is
  `<root>/ai/<current>/bitty-ai`, named by the descriptor's `executable`
  field; no host path, username, socket name, or repo URL is hardcoded. Roots
  arrive from configuration/environment (`HelperConfig::user_root` /
  `system_root`), so tests run in hermetic scratch directories.
- The resolved descriptor's `protocol` range is validated against Core's
  native-component protocol version and its version must equal the `current`
  pointer.

## 2. Digest verification

- The executable's SHA-256 is re-computed and compared against the pinned
  `bitty-component.toml` `sha256` on **every** `ensure_running` call, before
  any process is created. A mismatch (or a missing/oversized/non-regular file)
  returns `HelperError::Resolve` and nothing spawns.
- `ResidentHelper::resolve` exposes the same check without spawning, so Core
  can fail closed before allocating anything.
- Tests: `digest_mismatch_fails_closed_before_spawn`,
  `missing_component_fails_closed_as_not_installed`.

## 3. Stdio-coprocess spawn and framing

- Spawn is a plain `std::process::Command` with a **cleared environment** plus
  `COMPONENT_ENV_ALLOWLIST` (never `PATH`), `current_dir` set to the version
  directory, and piped stdin/stdout/stderr.
- Framing is the **accepted bitty-ipc framing** (`bitty-ipc-api` `frame`): a
  4-byte big-endian length prefix with a 256 KiB payload bound, used through
  `Framer`/`encode_frame`. Core frames and transports bytes only; envelope
  validation, wire-version negotiation, scopes, consent, and the six-gate
  dispatcher stay helper-side.
- One **single fenced writer** thread owns the child's stdin for one process
  generation; one reader thread decodes framed stdout into a bounded channel
  and one thread captures stderr into a bounded ring. Reader frames carry the
  process generation and a superseded generation's frames are discarded, so a
  replaced process can never feed the live session.
- Backpressure is structural: a full inbound channel blocks the reader, which
  stops draining the child's stdout pipe; `submit_frame` returns
  `HelperError::Busy` when the outbound queue is full rather than blocking the
  caller.

## 4. Supervision and restart contract

- **Resident singleton, never idle-stopped.** Unlike the `net` broker there is
  no idle timeout: the helper is expected to live for the whole Core session so
  the S-1 read-only service-metadata snapshot can be held against a versioned
  helper-side snapshot. Test: `resident_helper_is_not_idle_stopped`.
- **At most one process.** `ensure_running` finishes any stopping generation
  before spawning the next, so two generations never overlap.
- **Bounded restarts.** An exit while not shutting down is a crash. Crashes
  route through `crate::component::CrashTracker` with the DIR-030 numbers:
  restart delay `COMPONENT_BACKOFF_INITIAL` (1 s) doubling to
  `COMPONENT_BACKOFF_MAX` (30 s), and `COMPONENT_CRASH_LIMIT` (5) crashes
  inside `COMPONENT_CRASH_WINDOW` (5 min) latch the helper
  `HelperState::Unavailable` until a new Core session. A crash inside the
  delay is refused with `HelperError::Backoff { retry_after }`. Test:
  `crash_restarts_are_bounded_and_latch_unavailable`.
- **Framing faults fail closed.** A frame header past the bound makes the
  reader stop and the supervisor take the crash path rather than buffering
  unboundedly. Test: `framing_fault_is_treated_as_a_crash`.
- **Typed outcomes.** Every path returns `HelperError` (resolve, backoff,
  unavailable, spawn, frame-too-large, busy, closed) or a
  `HelperEvent::Exited(StopOutcome)`; there is no panic path. A crash logs only
  the escaped stderr tail.
- **Shutdown.** `ResidentHelper::shutdown` drops stdin (EOF), waits up to the
  DIR-030 shutdown grace, then kills the recorded child only. `Drop` never
  leaks a live child.

## Test evidence

- `crates/bitty-runtime/tests/ai_helper_spawn.rs` — 7 hermetic integration
  tests against `bitty-ai-helper-fixture` installed as `bitty-ai`: missing
  install, digest mismatch fail-closed, resolution version, successful spawn
  and frame round-trip, residency (no idle stop), bounded/latched restarts, and
  framing-fault crash.
- `crates/bitty-runtime/src/ai_helper.rs` unit tests — framing bound,
  closed-without-process, no-root, allowlist, and DIR-030 policy parity.
- The fixture speaks the real bitty-ipc framing and performs no network I/O; it
  is never installed or packaged.

## Out of scope (helper side / follow-ups)

- The 13-method `ai.*` surface, scopes, consent, budgets, and the snapshot
  version correlator live helper-side (`AI-0302`, slices E0..E3) and are not
  here.
- Core's wiring of `ResidentHelper` into the application event loop and the
  Lua/IPC request surface that will drive `submit_frame` is a follow-up slice.
- Registry/download install sources for the `ai` component remain a follow-up
  (only local installs exist today).
