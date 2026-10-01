# SEC-15 audit: R-018 unsafe/FFI discipline (CTX-0633, Issue #1084)

Source: risk-register.md R-018; P0-AC-033 (Unsafe/FFI discipline).
Scope: the `gpu.rs` and `bitty-winjob` `ffi.rs` unsafe allowances, lint
gate, boundary fuzzers.
Branch: `ctx-0633/sec15-unsafety`. Risk state stays `Open`: this record is
`Implemented`-only evidence pending independent auditor review per RS-1..RS-7.

## Method

Workspace-wide `rg` over `crates/*/src` and `crates/*/tests` for `unsafe {`,
`unsafe fn`, `unsafe impl`, `extern "`, `allow(unsafe_code)`, plus `transmute`,
`from_raw`, raw-pointer, and `build.rs` sweeps; crate-root lint-attribute
census; `deny.toml`, CI clippy/deny/audit steps; `fuzz/` target list;
`bitty-lua` dependency pins.

## Inventory (verified 2026-09-22, worktree `ctx-0633`)

Production (`src/`) `unsafe`, the full allowlist (20 blocks, 2 modules):

| #   | Location                                                | Form                                             | SAFETY rationale                                                                                                           | Verdict                    |
| --- | ------------------------------------------------------- | ------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- | -------------------------- |
| 1   | `crates/bitty-render/src/gpu.rs:293` (`create_surface`) | `unsafe { instance.create_surface_unsafe(...) }` | Raw display/window handles originate from a live `SurfaceTarget`; returned `Surface` owns a clone keeping the window alive | Keep, sole allowance       |
| 2   | `crates/bitty-render/src/gpu.rs:302` (`create_surface`) | `unsafe { transmute(surface) }` to `'static`     | Same ownership argument: `SurfaceKind::Gpu` stores the `SurfaceTarget` clone, so the window outlives the surface           | Keep, documented this task |

### `bitty-winjob` Win32 Job Object adapter (CTX-0903, DEC-0083)

Second allowance, added for the Windows owned-process-tree backend (#1536).
`crates/bitty-winjob/src/lib.rs` carries `#![deny(unsafe_code)]` and a
single `#[cfg(windows)] #[allow(unsafe_code)] mod ffi;`; every block below
is in `crates/bitty-winjob/src/ffi.rs`, compiled only on Windows, and
carries a `// SAFETY:` comment. Bindings come from `windows-sys` 0.61.2
(already locked; MIT OR Apache-2.0); the crate declares no `extern` block
of its own. Callers (`bitty-pty` `tree/windows.rs`, still
`forbid(unsafe_code)`) see only safe types holding `OwnedHandle`s.

| #   | Location (`ffi.rs`)                 | Call                                                            | SAFETY rationale                                                                                                          | Verdict |
| --- | ----------------------------------- | --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ------- |
| 3   | `:80` (`owned_or_null`)             | `OwnedHandle::from_raw_handle`                                  | Called only after the NULL sentinel check; the fresh handle is owned by nobody else and closed once                       | Keep    |
| 4   | `:98` (`create_kill_on_close_job`)  | `CreateJobObjectW(NULL, NULL)`                                  | NULL attributes/name are documented valid (default DACL, non-inheritable, anonymous); result checked                      | Keep    |
| 5   | `:105` (`create_kill_on_close_job`) | `SetInformationJobObject` (extended limits)                     | Live owned job handle; stack-local initialized struct; length is its exact `size_of` (const-checked)                      | Keep    |
| 6   | `:120` (`open_member`)              | `OpenProcess(SET_QUOTA\|TERMINATE\|QUERY_LIMITED\|SYNCHRONIZE)` | Value arguments only; minimal rights; NULL checked                                                                        | Keep    |
| 7   | `:128` (`assign`)                   | `AssignProcessToJobObject`                                      | Both handles borrowed from live `OwnedHandle`s; process handle has the required rights                                    | Keep    |
| 8   | `:136` (`terminate_job`)            | `TerminateJobObject`                                            | Borrowed live job handle created with default (all) access                                                                | Keep    |
| 9   | `:147` (`active_processes`)         | `QueryInformationJobObject` (basic accounting)                  | Stack-local writable struct with exact `size_of` length; optional return-length pointer NULL                              | Keep    |
| 10  | `:164` (`has_exited`)               | `WaitForSingleObject(h, 0)`                                     | Borrowed live handle with `SYNCHRONIZE`; zero timeout never blocks; every result value mapped                             | Keep    |
| 11  | `:185` (`exit_code`)                | `GetExitCodeProcess`                                            | Borrowed live handle with query rights; out-param is a stack `u32`; only read after the wait signaled                     | Keep    |
| 12  | `:193` (`process_is_running`)       | `OpenProcess(QUERY_LIMITED\|SYNCHRONIZE)`                       | Value arguments only; NULL checked (`ERROR_INVALID_PARAMETER` means no such process)                                      | Keep    |
| 13  | `:208` (`terminate_process`)        | `OpenProcess(TERMINATE)`                                        | Value arguments only; NULL checked                                                                                        | Keep    |
| 14  | `:211` (`terminate_process`)        | `TerminateProcess`                                              | Handle opened just above with `PROCESS_TERMINATE`, closed on drop                                                         | Keep    |
| 15  | `:219` (`resume_threads`)           | `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)`                | Value arguments only; `INVALID_HANDLE_VALUE` sentinel checked before wrapping                                             | Keep    |
| 16  | `:225` (`resume_threads`)           | `OwnedHandle::from_raw_handle` (snapshot)                       | Sentinel checked; fresh unshared handle, closed once                                                                      | Keep    |
| 17  | `:233` (`resume_threads`)           | `Thread32First`                                                 | Live snapshot; stack `THREADENTRY32` with `dwSize` preset to its exact size                                               | Keep    |
| 18  | `:253` (`resume_threads`)           | `Thread32Next`                                                  | Same snapshot and entry; kernel keeps `dwSize`                                                                            | Keep    |
| 19  | `:269` (`resume_thread`)            | `OpenThread(THREAD_SUSPEND_RESUME)`                             | Value arguments only; NULL checked; `ERROR_INVALID_PARAMETER` (thread exited after the snapshot) is skipped by the caller | Keep    |
| 20  | `:272` (`resume_thread`)            | `ResumeThread`                                                  | Handle opened just above with `THREAD_SUSPEND_RESUME`; `(DWORD)-1` failure sentinel checked                               | Keep    |

Boundary properties: inputs are pids, exit codes, and handles the module
created itself; no caller buffer, string, or length crosses the boundary,
and no raw `HANDLE` or pointer is returned. Every error is read with
`io::Error::last_os_error()` directly after the failing call. Boundary
fuzzing is therefore not applicable (nothing is parsed); coverage is the
Windows-only unit tests in `crates/bitty-winjob/src/job.rs` (pid 0
refused on every entry point, empty job, assign-before-resume, tree kill,
repeatable non-reaping exit observation, kill-on-close, resume of a gone
process fails) plus `crates/bitty-pty/tests/owned_tree_windows.rs`. Both
run only on the Windows CI job; the Linux host cross-checks them
(`cargo check`/`clippy --target x86_64-pc-windows-msvc`) but cannot
execute them.

No `extern "..."` blocks, no `build.rs`, no raw-pointer dereference, no
`mem::forget`/`ManuallyDrop` in any `src/` (the `extern "bitty"` hit in
`bitty-terminal/src/completion.rs` is Nushell text inside a string
literal). `from_raw_*` hits are safe
newtype constructors (`WindowId`, `JobId`, OS-error mapping); `ptr::eq`
hits are safe identity comparisons in tests.

Test-only `unsafe` (each under explicit `#![allow]`/`#[allow]` with comment):

| Location                                           | Use                                                      | Why safe here                                                                     |
| -------------------------------------------------- | -------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `bitty-pty/tests/spawn_smoke.rs:304,314`           | `std::env::set_var`/`remove_var` (edition-2024 `unsafe`) | Single-threaded test, poison window narrowed to spawn, restored immediately after |
| `bitty-rich/tests/background_peak_memory.rs:74-97` | `unsafe impl GlobalAlloc` delegating to `System`         | Trait requires `unsafe`; counting only, no layout tricks                          |
| `bitty-runtime/tests/soak.rs:710`                  | `Waker::from_raw` noop waker, null data pointer          | All vtable entries no-op; waker never wakes                                       |

## Lint gate (P0-AC-033 clause 3: PASS)

- Workspace `Cargo.toml [workspace.lints.rust] unsafe_code = "deny"`; every
  member sets `[lints] workspace = true`, so deny covers all targets
  (lib, bins, examples, tests, benches).
- Crate-level `#![forbid(unsafe_code)]` on every crate root except
  `bitty-render` (`#![deny]` + `#[allow(unsafe_code)] pub mod gpu;` only)
  and `bitty-winjob` (`#![deny]` + `#[cfg(windows)] #[allow(unsafe_code)]
mod ffi;` only).
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
- Negative control: `rg 'unsafe \{|unsafe fn|unsafe impl|allow\(unsafe_code\)' crates/*/src`
  matches only the allowlisted `gpu.rs` pair, the `bitty-render` and
  `bitty-winjob` module allowances, and the 18 `bitty-winjob/src/ffi.rs`
  blocks above (CTX-0903). The broader `rg 'unsafe|extern "'` additionally
  hits prose comments and the Nushell `extern` string.
- CTX-0903: `cargo clippy -p bitty-winjob -p bitty-pty -p bitty-runtime --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings`
