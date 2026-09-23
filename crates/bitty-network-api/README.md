# `bitty-network-api`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-network-api` is the stable network vocabulary: HTTP
`Request`/`Response`, `WebSocketRequest`, capability definitions
(`NetworkCapability` domain allowlist, `OfflineFirst` policy marker),
`NetworkError`, and the `NetworkService` trait (`request` + `websocket`
signatures). Types only — no implementation, no I/O, no sockets; sockets
and transports arrive in follow-up tasks behind the trait.

## No implementation dependencies

The manifest is dependency-free (`std` only) and the crate carries
`#![forbid(unsafe_code)]`. Nothing here can move a byte: there is no socket
code, no background task, and no transport, TLS, or DNS implementation.

## Consumer rule

Plugins and every other consumer depend on `bitty-network-api` only for the
vocabulary (capability checks, `NetworkError` handling, request building).
Reach the implementation exclusively via IPC or service lookup through
`NetworkService` — never depend on `bitty-network` directly. Transports
evolve behind the trait while this surface stays stable.

## Default-off

Networking is off by default. `NetworkCapability::offline()` is deny-all and
every consumer starts there; a domain becomes reachable only when explicitly
added with `with_domain` (exact match, no wildcards). There is no global
"enable network" switch in this crate. Check `Request::host` /
`WebSocketRequest::host` with `NetworkCapability::check` before calling the
service, and treat `Offline`/`Denied` as normal fail-closed control flow.

## Boundaries

- Zero dependencies, per `Cargo.toml`; `std` only.
- No I/O, no sockets, no background tasks.
- Zero `unsafe`, per the workspace lint and `src/lib.rs`.

## Layout

- `Cargo.toml` — package metadata (no dependencies).
- `src/lib.rs` — request/response types, capability, error, policy, and
  service-trait definitions plus unit tests.
