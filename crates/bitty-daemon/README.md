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
(bitty#2606, CTX-1616).

## Status: seam only

This crate fixes the service version, the capability set, and the
version-negotiation skeleton. It **starts no listener**, binds no endpoint,
creates no socket file, and owns no session state. `bittyd` with no arguments
reports that seam and exits 0.

The contract areas that follow are deliberately deferred to their owning
slices:

| Area                                                                                                                                        | Owner      |
| ------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| Instance/session ids, registry, generation and revision counters, retention, orphan records                                                 | bitty#2601 |
| `bitty-ipc` v1 server loop, peer capture and re-check, scope evaluation, endpoint lock and tamper detection, method dispatch, error mapping | bitty#2607 |
| Advertised limits and compiled system maxima                                                                                                | bitty#2603 |
| Endpoint discovery and platform cells                                                                                                       | bitty#2604 |
| Leases, control token, attach/detach, abort/kill                                                                                            | bitty#2609 |
| Event stream, backlog, heartbeat                                                                                                            | bitty#2602 |
| Conformance corpus and fake-server parity                                                                                                   | bitty#2605 |

Nothing here authorizes autostart, a service file, a TCP listener, or desktop
wiring; all four stay out of scope.

## Frozen contract values

Source: contract decisions `D-01`, `D-08`, `D-11`, and the "Handshake" section
of the service surface.

| Item                 | Value                                                                                                                      | Where                           |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------- |
| Service API version  | `{major: 1, minor: 0}`                                                                                                     | `contract::SERVICE_API_VERSION` |
| Minimum client minor | `0` (`1.0..=1.0` accepted)                                                                                                 | `contract::MIN_CLIENT_MINOR`    |
| Capabilities         | `terminal_sessions`, `events`, `control_transfer`, `idempotent_mutations` enabled; `worker_sessions` disabled              | `contract::CAPABILITIES`        |
| Handshake method     | `daemon.negotiate`                                                                                                         | `contract::NEGOTIATE_METHOD`    |
| Negotiation rule     | same major, client minor in `[min_client_minor, server_minor]`; every other case is `VersionMismatch` before any operation | `handshake::negotiate`          |
| Worker sessions      | `false` in every v0.2.0 build (`D-01`); enabling needs an ADR 0019 amendment and a dedicated security review               | `contract::CAPABILITIES`        |

`handshake::NegotiateResponse` carries only the version and capability facts
this seam owns. The remaining response fields (`daemon_instance_id`,
`daemon_pid`, `server_identity`, `peer_identity`, `client_id`, `scopes`,
`limits`, `server_monotonic_ms`, `daemon_state`) and the optional request
fields (`client_instance`, `operator_capability`) are listed in the module
docs and arrive with bitty#2601/#2603/#2607. `operator_capability` stays out of
this crate until the slice that owns its secret lifetime and storage exists.

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
- the crate has anything other than one library and exactly one binary
  (`bittyd`).

A daemon-shaped global in a terminal or mechanism crate is a contract
violation, not a wiring detail (ADR 0008 / ADR 0019).

## Layout

- `Cargo.toml` — package metadata, one `[[bin]]` (`bittyd`), no runtime
  dependency yet.
- `src/lib.rs` — crate docs, status, and the deferral map.
- `src/contract.rs` — service version, capability set, method name.
- `src/handshake.rs` — `daemon.negotiate` request/response skeleton and the
  `D-11` acceptance rule.
- `src/seam.rs` — explicit no-listener/no-endpoint status.
- `src/cli.rs` — pure argument parsing and rendering, exit codes.
- `src/main.rs` — thin binary entry point.
- `tests/dependency_direction.rs` — one-way dependency and crate-shape gate.
- `tests/binary.rs` — `--version`, no-argument seam, no runtime files, usage
  error.

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
