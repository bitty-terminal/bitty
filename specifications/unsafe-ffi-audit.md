# SEC-15 audit: R-018 unsafe/FFI discipline (CTX-0633, Issue #1084)

Source: risk-register.md R-018; P0-AC-033 (Unsafe/FFI discipline).
Scope: sole `gpu.rs` unsafe allowance, lint gate, boundary fuzzers.
Branch: `ctx-0633/sec15-unsafety`. Risk state stays `Open`: this record is
`Implemented`-only evidence pending independent auditor review per RS-1..RS-7.

## Method

Workspace-wide `rg` over `crates/*/src` and `crates/*/tests` for `unsafe {`,
`unsafe fn`, `unsafe impl`, `extern "`, `allow(unsafe_code)`, plus `transmute`,
`from_raw`, raw-pointer, and `build.rs` sweeps; crate-root lint-attribute
 census; `deny.toml`, CI clippy/deny/audit steps; `fuzz/` target list;
 `bitty-lua` dependency pins.

## Inventory (verified 2026-09-22, worktree `ctx-0633`)

Production (`src/`) `unsafe`, the full allowlist (2 blocks, 1 module):

| # | Location | Form | SAFETY rationale | Verdict |
|---|----------|------|------------------|---------|
| 1 | `crates/bitty-render/src/gpu.rs:293` (`create_surface`) | `unsafe { instance.create_surface_unsafe(...) }` | Raw display/window handles originate from a live `SurfaceTarget`; returned `Surface` owns a clone keeping the window alive | Keep, sole allowance |
| 2 | `crates/bitty-render/src/gpu.rs:302` (`create_surface`) | `unsafe { transmute(surface) }` to `'static` | Same ownership argument: `SurfaceKind::Gpu` stores the `SurfaceTarget` clone, so the window outlives the surface | Keep, documented this task |

No `extern "..."` blocks, no `build.rs`, no raw-pointer dereference, no
`mem::forget`/`ManuallyDrop` in any `src/`. `from_raw_*` hits are safe
newtype constructors (`WindowId`, `JobId`, OS-error mapping); `ptr::eq`
hits are safe identity comparisons in tests.

Test-only `unsafe` (each under explicit `#![allow]`/`#[allow]` with comment):

| Location | Use | Why safe here |
|----------|-----|---------------|
| `bitty-pty/tests/spawn_smoke.rs:304,314` | `std::env::set_var`/`remove_var` (edition-2024 `unsafe`) | Single-threaded test, poison window narrowed to spawn, restored immediately after |
| `bitty-rich/tests/background_peak_memory.rs:74-97` | `unsafe impl GlobalAlloc` delegating to `System` | Trait requires `unsafe`; counting only, no layout tricks |
| `bitty-runtime/tests/soak.rs:710` | `Waker::from_raw` noop waker, null data pointer | All vtable entries no-op; waker never wakes |

## Lint gate (P0-AC-033 clause 3: PASS)

- Workspace `Cargo.toml [workspace.lints.rust] unsafe_code = "deny"`; every
  member sets `[lints] workspace = true`, so deny covers all targets
  (lib, bins, examples, tests, benches).
- Crate-level `#![forbid(unsafe_code)]` on every crate root except
  `bitty-render` (`#![deny]` + `#[allow(unsafe_code)] pub mod gpu;` only).
  This task added the two missing pins: `bitty-core`, `bitty-test-support`.
- `just check` runs `cargo clippy --workspace --all-targets --locked --
  -D warnings`; CI repeats it plus `cargo deny check` and `cargo audit`.
  An undocumented `unsafe` anywhere fails the gate (deny + `-D warnings`).

## Lua adapter (R-018 Lua leg)

`bitty-lua` has `#![forbid(unsafe_code)]` and **no `mlua` dependency**;
the VM is `phodopus` rev-pinned (`1653c51f...`, exact-rev git pin per
`deny.toml [sources]`). Transitive `unsafe` inside the `phodopus`/`gc-arena`
VM is out of tree and unaudited here — recorded as residual gap G-3 below.
The evidence-matrix line about "`mlua` `vendored` unsafe confined narrow
adapters" does not match the tree (no `mlua` present); the matrix needs a
correction follow-up, not a code change.

## Fixes applied in this task (fail-closed, small)

1. Added missing `#![forbid(unsafe_code)]` pins: `bitty-core/src/lib.rs`,
   `bitty-test-support/src/lib.rs`.
2. Corrected stale "single `unsafe` block" claims to two blocks in
   `bitty-render/src/gpu.rs` module docs and `bitty-render/src/lib.rs`
   crate docs (the `transmute` block was uncounted; both carry SAFETY
   comments and stay inside `gpu::Surface` construction).

## Residual gaps (follow-up tasks, NOT fixed here)

- G-1 Boundary fuzzers: `fuzz/` covers only the VT family (`vt_parser`,
  `osc_string`, `dcs_apc_string`) plus the R-002 rich corpus. No fuzzers
  exist for the PTY spawn/read seam, the `SurfaceTarget`/raw-handle seam,
  font rasterization, winit event mapping, or the Lua host boundary.
  P0-AC-033 clause 4 is not met; needs a fuzzer-per-seam task.
- G-2 `transmute` hardening: the `'static` lifetime extension in
  `create_surface` is sound only while `Surface` always owns the target
  clone. A `debug_assert!`/type-state or a `compile_fail` test pinning
  `SurfaceKind::Gpu { target }` co-ownership would make regressions
  fail-closed; suggested follow-up.
- G-3 Transitive Lua-VM `unsafe`: `phodopus` + `gc-arena` (git deps)
  contain upstream `unsafe` outside this workspace; needs a focused
  upstream-version audit or a `cargo geiger`-style transitive count task.
- G-4 Evidence-matrix drift: the R-018 row's "`mlua` `vendored`" text is
  stale (bitty-docs correction, out of scope for this repo).

## Verification

- `cargo test -p bitty-core -p bitty-test-support -p bitty-render`
- `cargo clippy -p bitty-core -p bitty-test-support -p bitty-render --all-targets --locked -- -D warnings`
- `cargo fmt --all -- --check`
- Negative control: `rg 'unsafe|extern "' crates/*/src` matches only the
  allowlisted `gpu.rs` pair after this task.
