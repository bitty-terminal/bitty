# `bitty-network`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-network` is the implementation shell behind `bitty-network-api`: a
module skeleton (`runtime`, `transport`, `protocol`, `tls`, `dns`, `policy`)
holding marker types only. Shell only — sockets arrive in a follow-up task
(see the sealing note in `src/lib.rs`).

## Embedded-first, networkd-later

The terminal stays embedded-first: no network daemon today, no background
tasks, no ambient connectivity. A `networkd` out-of-process owner may arrive
later; until then every consumer starts offline and this crate moves no
bytes. Its only dependency is the path-local `bitty-network-api` vocabulary.

## API-stable promise

Transports evolve here, behind `NetworkService`. The vocabulary in
`bitty-network-api` stays stable: additive changes only, no renames and no
signature breaks without a dedicated task and a migration note. Consumers
depend on `-api` only and reach this crate via IPC or service lookup —
never as a direct dependency.

## Features

All default-off, all empty (no new dependencies yet):

- `client`, `server` — initiator/listener roles.
- `http`, `websocket` — protocol wire code.
- `quic` — QUIC transport.
- `proxy`, `oauth` — egress policy and credential flows.

## Boundaries

- One dependency: `bitty-network-api` via path; `std` otherwise.
- No I/O, no sockets, no background tasks.
- Zero `unsafe`, per the workspace lint and `src/lib.rs`.

## Layout

- `Cargo.toml` — package metadata, path dependency on `-api`, empty
  default-off features.
- `src/lib.rs` — shell docs plus `-api` re-exports.
- `src/runtime.rs` — executor-ownership marker.
- `src/transport.rs` — TCP/UDP/QUIC markers.
- `src/protocol.rs` — HTTP/WS markers.
- `src/tls.rs` — TLS marker.
- `src/dns.rs` — resolver marker.
- `src/policy.rs` — capability/policy re-exports.
