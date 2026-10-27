# `bitty-daemon`

> Part of the `bitty` workspace. Canonical platform documentation lives in
> `bitty-terminal-docs` (mounted at `docs/`) and shared governance in
> `bitty-docs`; this file is a crate-local map, not a canonical contract.

## Purpose

`bitty-daemon` is the crate seam for `bittyd`, the per-user, single-instance
session host admitted to the v0.2.0 slice by
[ADR 0019](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0019-bittyd-staging-advance.md)
and bounded by
[ADR 0008](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/decisions/adrs/ADR-0008-headless.md).
The normative service surface is the accepted
[`bittyd` Session-Host Contract](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md)
(contract tracked by bitty#2546/#2663); this crate seam is bitty#2606
(CTX-1616).

## Status: seam plus the in-memory registry core

This crate fixes the service version, the capability set, and the
version-negotiation skeleton, and implements the registry half of bitty#2601:
server-generated ids, the in-memory session registry, generation and revision
counters, retention, and the bounded orphan journal. It **starts no listener**,
binds no endpoint, creates no socket file, spawns no worker, and serves no IPC
method. `bittyd` with no arguments still reports the no-endpoint seam and exits
0; the registry is a library surface for bitty#2607 to drive.

The contract areas that follow are deliberately deferred to their owning
slices:

| Area                                                                                                                                        | Owner      |
| ------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| `bitty-ipc` v1 server loop, peer capture and re-check, scope evaluation, endpoint lock and tamper detection, method dispatch, error mapping | bitty#2607 |
| Advertised limits and compiled system maxima                                                                                                | bitty#2603 |
| Endpoint discovery, cgroup/subreaper containment, production orphan reaper, executable path digest, non-Linux probes and Windows ACLs       | bitty#2604 |
| Leases, control token, attach/detach authority, abort/kill                                                                                  | bitty#2609 |
| Event stream, backlog, heartbeat                                                                                                            | bitty#2602 |
| Conformance corpus and fake-server parity                                                                                                   | bitty#2605 |

Nothing here authorizes autostart, a service file, a TCP listener, or desktop
wiring; all four stay out of scope.

## Frozen contract values

Source: contract decisions `D-01`, `D-08`, `D-11`, and the "Handshake" section
of the service surface.

| Item                 | Value                                                                                                                            | Where                           |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------- |
| Service API version  | `{major: 1, minor: 0}`                                                                                                           | `contract::SERVICE_API_VERSION` |
| Minimum client minor | `0` (`1.0..=1.0` accepted)                                                                                                       | `contract::MIN_CLIENT_MINOR`    |
| Capabilities         | `terminal_sessions`, `events`, `observer_grants`, `control_transfer`, `idempotent_mutations` enabled; `worker_sessions` disabled | `contract::CAPABILITIES`        |
| Handshake method     | `daemon.negotiate`                                                                                                               | `contract::NEGOTIATE_METHOD`    |
| Negotiation rule     | same major, client minor in `[min_client_minor, server_minor]`; every other case is `VersionMismatch` before any operation       | `handshake::negotiate`          |
| Worker sessions      | `false` in every v0.2.0 build (`D-01`); enabling needs an ADR 0019 amendment and a dedicated security review                     | `contract::CAPABILITIES`        |

## Registry core (bitty#2601)

`registry` is the in-memory session registry. It starts no listener, touches no
file, and owns only metadata.

- `DaemonInstanceId` and `SessionId` are 128-bit OS-CSPRNG values encoded as 32
  lowercase hex characters. Only the registry mints ids; a parsed value is an
  echo for lookup, never authority. Entropy failure refuses the operation.
  Every create mints a fresh value, a collision with a live or retained record
  is regenerated, and a collected id is never recycled as a new value.
- `SessionRegistry::create` starts at generation `0` and state revision `0`;
  `bind` increments the generation exactly once (v0.2.0 permits one bind);
  `attach`/`detach` drive the derived attach count; `heartbeat` updates
  last-seen without a revision bump; `begin_stopping` and `terminate` apply one
  revision bump per snapshot-visible transition. Counters use checked
  arithmetic and fail closed.
- Terminal states are immutable. A terminal record is retained (never live,
  never revived) until `terminal_retention_ms` or
  `max_retained_terminal_records` collects it in deterministic
  `(terminalized_at, session_id)` order.
- A live session's `terminal_id` is unique; a retained terminal record may share
  it. Refusals (`max_sessions`, `session_record_bytes`, duplicate terminal id)
  are whole: no partial record, no counter change.
- Enforced bounds are compiled from `D-14`: `max_sessions` 16/64,
  `max_retained_terminal_records` 64, `session_record_bytes` 4096,
  `terminal_retention_ms` 600000/3600000. Advertising them as
  `{effective, default, system_max}` belongs to bitty#2603.

`orphan` is the two-phase, bounded orphan journal. `reserve` fsyncs a
`reserved` record (session id, generation, boot id, reserved-at) before the
spawn; `commit` fills the exact pid, process start time, and process group after
the spawn returns and fsyncs again, before the worker can be exposed as bound.
The journal holds at most `max_orphan_records` records of at most
`orphan_record_bytes`, contains no argv, environment, cwd, content, or
credential, and on Unix lives in a `0700` directory with `0600` files.

Classification is conservative and never signals by pid:

| Observation                                                            | Outcome                                      |
| ---------------------------------------------------------------------- | -------------------------------------------- |
| Exact boot id, pid, start time, and process group                      | only this reaches `OrphanReaper`             |
| Different boot id                                                      | `stale`, discarded, never signalled          |
| Reserved record with no pid                                            | `unverified`, quarantined, never signalled   |
| Absent pid, unreadable start time, reused start time, group mismatch   | `unverified`, quarantined, never signalled   |
| Reaper cannot verify the whole containment identity                    | `unverified`, quarantined                    |

Quarantined records expire after `orphan_record_retention_ms`; operator
retirement removes a quarantined record explicitly and never signals.
`VerifiedOrphan` has a private constructor, so a reaper can only be asked to
terminate an exact match.

Windows: the journal compiles and enforces the same record bound, but this slice
does not create a current-user-only DACL and has no boot-id/`/proc` probe.
Until bitty#2604 supplies those platform cells, opening a journal requires a
caller-supplied boot id and every record classifies as unverified, so nothing is
signalled. The journal inherits the ACL of the daemon-owned runtime directory;
bitty#2604 owns that directory and its current-user-only DACL.

`handshake::NegotiateResponse` carries only the version and capability facts
this seam owns. The remaining response fields (`wire_version` (transport-owned),
`daemon_instance_id`, `daemon_pid`, `server_identity`, `peer_identity`,
`client_id`, `scopes`, `limits`, `server_monotonic_ms`, `daemon_state`) and the
optional request fields (`client_correlation`, `operator_capability`,
`host_grant`) are listed in the module docs and arrive with
bitty#2601/#2603/#2607. `client_correlation` is bounded to 128 bytes of valid
UTF-8 with no control characters and is recorded only as a truncated 128-bit
SHA-256 digest (`D-08`); `operator_capability` and `host_grant` stay out of this
crate until the slice that owns their secret lifetime and storage exists.

## CLI

```text
bittyd --version   # crate version + contract service API, exit 0
bittyd --help      # usage, exit 0
bittyd             # seam status (no endpoint bound), exit 0
bittyd <other>     # usage error, exit 2
```

Exit codes 3 through 6 are reserved by the contract for `daemon.stop` modes and
endpoint tamper. The seam cannot reach them: it hosts no session and binds no
endpoint.

## Dependency direction

The daemon sits **above** the mechanism crates. It may depend on
`bitty-term-state`, `bitty-pty`, `bitty-plugin-host`, `bitty-ipc`, and other
mechanism crates; none of them may depend on `bitty-daemon`. The seam declares
no mechanism edge yet; each edge is added by the slice that consumes it
(bitty#2601 for registry state, bitty#2607 for IPC, bitty#2604 for platform
cells).

`tests/dependency_direction.rs` reads `cargo metadata --no-deps` and fails
when:

- any non-daemon crate depends on `bitty-daemon` (normal, dev, build,
  optional, or target-specific edge);
- `bitty-daemon` is not a workspace member;
- the crate has other than one library and one binary (`bittyd`); test targets
  are allowed and are not counted.

A daemon-shaped global in a terminal or mechanism crate is a contract
violation, not a wiring detail (ADR 0008 / ADR 0019).

## Layout

- `Cargo.toml` — package metadata, one `[[bin]]` (`bittyd`), no runtime
  dependency yet.
- `src/lib.rs` — crate docs, status, and the deferral map.
- `src/contract.rs` — service version, capability set, method name.
- `src/handshake.rs` — `daemon.negotiate` request/response skeleton and the
  `D-11` acceptance rule.
- `src/registry/` — ids, session records and lifecycle, the registry table,
  counters, retention, and refusals.
- `src/orphan/` — process identity probes, the bounded journal, classification,
  and the exact-identity reaper seam.
- `src/seam.rs` — explicit no-listener/no-endpoint status.
- `src/cli.rs` — pure argument parsing and rendering, exit codes.
- `src/main.rs` — thin binary entry point.
- `tests/dependency_direction.rs` — one-way dependency and crate-shape gate.
- `tests/binary.rs` — `--version`, no-argument seam, no runtime files, usage
  error.
- `tests/registry.rs` — public registry lifecycle, ids, refusals, limits.
- `tests/orphan_journal.rs` — public journal reserve/commit/reap, no-signal
  paths, owner-private Unix files, and the no-argv/env record shape.

## Gates

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p bitty-daemon --locked
cargo check --workspace --all-targets --locked
```

`just daemon-seam-test` runs the crate's suite; it is part of `just check`,
and the workspace `cargo nextest run --workspace` in CI discovers the same
tests.
