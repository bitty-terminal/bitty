# `bitty-sandbox`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-sandbox` is the narrow inherited-OS-confinement adapter for
plugin-spawned tools: on Linux it applies a Landlock ABI v1 `EXECUTE`
ruleset in the child before exec so an admitted tool cannot exec outside
its allowlist no matter which escape hatch the admitted binary offers;
every other platform compiles to the portable policy surface and refuses
the spawn fail-closed. Confinement setup failure refuses the spawn with a
diagnostic, never a silent pass (`src/lib.rs`, `src/linux.rs`).

## Boundaries

- Sole syscall dependency, per `Cargo.toml`: `libc` 0.2 line on Linux only
  (already pinned workspace-wide); no new supply-chain surface and no
  `landlock` crate in the tree — ABI v1 calls go through raw
  `libc::syscall` numbers.
- Separate crate so the raw syscalls stay behind their own boundary:
  the crate root denies `unsafe_code` and only the private Linux module
  allows it, each `unsafe` block carrying a `SAFETY` rationale, following
  the `bitty-winjob` precedent. The public surface is a safe API.

## Status

Accepted contracts, per `src/lib.rs`: CTX-1151 (#1984) inherited-OS
confinement for admitted tool spawns. macOS (`sandbox-exec` profile) and
Windows (Job Object / restricted token) enforcement stays a sequel; both
refuse fail-closed today.

## Layout

- `Cargo.toml` — package metadata and the Linux-only `libc` edge.
- `src/lib.rs` — crate docs with the posture table and safe API.
- `src/linux.rs` — Landlock ABI v1 enforcement shim.
