# `bitty-net`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-net` is the network dependency seam: it owns the capability, error,
and policy types (`NetworkCapability`, `NetworkError`, `OfflineFirst`) that
future network consumers will depend on. Shell only — sockets arrive in a
follow-up task (see the sealing note in `src/lib.rs`).

## Default-off

Networking is off by default. `NetworkCapability::offline()` is deny-all and
every consumer starts there; a domain becomes reachable only when explicitly
added with `with_domain` (exact match, no wildcards). There is no global
"enable network" switch in this crate.

## Consumer order

Future consumers adopt the seam in this order:

1. Depend on `bitty-net` for the vocabulary only (capability checks,
   `NetworkError` handling) — no socket code yet.
2. Thread a `NetworkCapability` from the composition root down to the call
   site; check with `allows`/`check` before any I/O.
3. Treat `Offline`/`Denied` as normal control flow (fail closed), and reserve
   `Timeout`/`Budget` handling for the socket follow-up that produces them.

## Boundaries

- Zero dependencies, per `Cargo.toml`; `std` only.
- No I/O, no sockets, no background tasks.
- Zero `unsafe`, per the workspace lint and `src/lib.rs`.

## Layout

- `Cargo.toml` — package metadata (no dependencies).
- `src/lib.rs` — capability, error, and policy types plus unit tests.
