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

| #   | Location (`ffi.rs`)           | Call                                                            | SAFETY rationale                                                                                                                                                                    | Verdict |
| --- | ----------------------------- | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| 3   | `:80` (`owned_or_null`)       | `OwnedHandle::from_raw_handle`                                  | Called only after the NULL sentinel check; the fresh handle is owned by nobody else and closed once                                                                                 | Keep    |
| 4   | (`create_job`)                | `CreateJobObjectW(NULL, NULL)`                                  | NULL attributes/name are documented valid (default DACL, non-inheritable, anonymous); result checked                                                                                | Keep    |
| 5   | (`create_job`, kill-on-close) | `SetInformationJobObject` (extended limits)                     | Live owned job handle; stack-local initialized struct; length is its exact `size_of` (const-checked); skipped for detached jobs (DEC-0102: fewer kernel semantics, no new `unsafe`) | Keep    |
| 6   | `:120` (`open_member`)        | `OpenProcess(SET_QUOTA\|TERMINATE\|QUERY_LIMITED\|SYNCHRONIZE)` | Value arguments only; minimal rights; NULL checked                                                                                                                                  | Keep    |
| 7   | `:128` (`assign`)             | `AssignProcessToJobObject`                                      | Both handles borrowed from live `OwnedHandle`s; process handle has the required rights                                                                                              | Keep    |
| 8   | `:136` (`terminate_job`)      | `TerminateJobObject`                                            | Borrowed live job handle created with default (all) access                                                                                                                          | Keep    |
| 9   | `:147` (`active_processes`)   | `QueryInformationJobObject` (basic accounting)                  | Stack-local writable struct with exact `size_of` length; optional return-length pointer NULL                                                                                        | Keep    |
| 10  | `:164` (`has_exited`)         | `WaitForSingleObject(h, 0)`                                     | Borrowed live handle with `SYNCHRONIZE`; zero timeout never blocks; every result value mapped                                                                                       | Keep    |
| 11  | `:185` (`exit_code`)          | `GetExitCodeProcess`                                            | Borrowed live handle with query rights; out-param is a stack `u32`; only read after the wait signaled                                                                               | Keep    |
| 12  | `:193` (`process_is_running`) | `OpenProcess(QUERY_LIMITED\|SYNCHRONIZE)`                       | Value arguments only; NULL checked (`ERROR_INVALID_PARAMETER` means no such process)                                                                                                | Keep    |
| 13  | `:208` (`terminate_process`)  | `OpenProcess(TERMINATE)`                                        | Value arguments only; NULL checked                                                                                                                                                  | Keep    |
| 14  | `:211` (`terminate_process`)  | `TerminateProcess`                                              | Handle opened just above with `PROCESS_TERMINATE`, closed on drop                                                                                                                   | Keep    |
| 15  | `:219` (`resume_threads`)     | `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)`                | Value arguments only; `INVALID_HANDLE_VALUE` sentinel checked before wrapping                                                                                                       | Keep    |
| 16  | `:225` (`resume_threads`)     | `OwnedHandle::from_raw_handle` (snapshot)                       | Sentinel checked; fresh unshared handle, closed once                                                                                                                                | Keep    |
| 17  | `:233` (`resume_threads`)     | `Thread32First`                                                 | Live snapshot; stack `THREADENTRY32` with `dwSize` preset to its exact size                                                                                                         | Keep    |
| 18  | `:253` (`resume_threads`)     | `Thread32Next`                                                  | Same snapshot and entry; kernel keeps `dwSize`                                                                                                                                      | Keep    |
| 19  | `:269` (`resume_thread`)      | `OpenThread(THREAD_SUSPEND_RESUME)`                             | Value arguments only; NULL checked; `ERROR_INVALID_PARAMETER` (thread exited after the snapshot) is skipped by the caller                                                           | Keep    |
| 20  | `:272` (`resume_thread`)      | `ResumeThread`                                                  | Handle opened just above with `THREAD_SUSPEND_RESUME`; `(DWORD)-1` failure sentinel checked                                                                                         | Keep    |

Boundary properties: inputs are pids, exit codes, and handles the module
created itself; no caller buffer, string, or length crosses the boundary,
and no raw `HANDLE` or pointer is returned. Every error is read with
`io::Error::last_os_error()` directly after the failing call. Boundary
fuzzing is therefore not applicable (nothing is parsed); coverage is the
Windows-only unit tests in `crates/bitty-winjob/src/job.rs` (pid 0
refused on every entry point, empty job, assign-before-resume, tree kill,
repeatable non-reaping exit observation, kill-on-close, detached survival
plus explicit detached kill, resume of a gone
process fails) plus `crates/bitty-pty/tests/owned_tree_windows.rs` (owned
kill-on-close drop plus detached handle-close survival). Both
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

---

## CTX-0978 addendum: native ConPTY job-at-creation spawn (issues #1653 + #1579)

Date: 2026-10-05. Scope: `crates/bitty-winjob` ConPTY spawn path (new
`conpty` module + 19 new `ffi.rs` blocks), `crates/bitty-pty` native
Windows backend, `Pty`-owned trees, `bitty-runtime` stored-tree kill.

### Why native (recorded, not re-litigated here)

`portable-pty` 0.9 spawns ConPTY children with fixed creation flags, so a
`OwnedTree::adopt` after the spawn leaves a window where an early
grandchild escapes the job (issue #1579). Upstream is stale (0.9.0 since
2025-02, past the ADR-0004 twelve-month rule), so per DEC-0101 the Windows
backend spawns natively through `bitty-winjob` with
`PROC_THREAD_ATTRIBUTE_JOB_LIST` at creation. The Unix backend still wraps
`portable-pty`; `bitty-pty` no longer depends on it on Windows.

### Inventory delta (verified 2026-10-05, worktree `carryctx/ctx-0978`)

`bitty-pty/src` still contains **zero** `unsafe` blocks
(`#![forbid(unsafe_code)]`, verified by `rg`). All 19 new blocks are in
`crates/bitty-winjob/src/ffi.rs`, each carrying a `// SAFETY:` comment.
`crates/bitty-winjob/src/conpty.rs` (safe orchestration plus the pure
UTF-16 builders) contains no `unsafe`; the crate root still denies
`unsafe_code` with the single `#[allow(unsafe_code)] mod ffi`.

| #   | Location (`ffi.rs`)                          | Call                                          | SAFETY rationale                                                                                                                                                                                                                             | Verdict |
| --- | -------------------------------------------- | --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------- |
| 21  | `:342` (`Drop for PseudoConsole`)            | `ClosePseudoConsole`                          | Handle came from a successful `CreatePseudoConsole`; `Drop` runs exactly once per value                                                                                                                                                      | Keep    |
| 22  | `:376` (`create_pseudo_console`)             | `CreatePseudoConsole`                         | Borrowed live pipe handles; stack `HPCON` slot; `HRESULT` checked (`S_OK`); failure carries the `HRESULT` (no last-error contract)                                                                                                           | Keep    |
| 23  | `:405` (`resize_pseudo_console`)             | `ResizePseudoConsole`                         | Borrowed live console; same `HRESULT` contract                                                                                                                                                                                               | Keep    |
| 24  | `:422` (`create_pipe`)                       | `CreatePipe`                                  | Stack `HANDLE` slots; NULL attributes (default, non-inheritable) and zero size documented valid; both handles wrapped before any fallible step                                                                                               | Keep    |
| 25  | `:426` (`create_pipe`)                       | `OwnedHandle::from_raw_handle` (read)         | Success checked above; fresh, exclusively owned; closed once; no fallible step between the two wraps                                                                                                                                         | Keep    |
| 26  | `:427` (`create_pipe`)                       | `OwnedHandle::from_raw_handle` (write)        | Same ownership argument as #25                                                                                                                                                                                                               | Keep    |
| 27  | `:455` (`AttributeList::new`, size query)    | `InitializeProcThreadAttributeList(NULL, ..)` | NULL list with a valid count only queries the size; nothing exists to free; `bytes` validated below instead of trusting the return                                                                                                           | Keep    |
| 28  | `:464` (`AttributeList::new`, init)          | `InitializeProcThreadAttributeList(buf, ..)`  | Buffer is exactly the queried size and lives in `self`; return value checked                                                                                                                                                                 | Keep    |
| 29  | `:493` (`set_pseudo_console`)                | `UpdateProcThreadAttribute(PSEUDOCONSOLE)`    | Live list; the `HPCON` travels by value in the pointer slot with `size_of::<HPCON>()` (API contract, `portable-pty` 0.9 parity); nothing outlives the call                                                                                   | Keep    |
| 30  | `:515` (`set_job`)                           | `UpdateProcThreadAttribute(JOB_LIST)`         | `job_slot` is owned by `self`, which outlives the spawn call; size is exactly one `HANDLE`; job handle is live                                                                                                                               | Keep    |
| 31  | `:532` (`Drop for AttributeList`)            | `DeleteProcThreadAttributeList`               | Initialized by `new`, deleted exactly once here                                                                                                                                                                                              | Keep    |
| 32  | `:574` (`spawn_conpty`)                      | `CreateProcessW`                              | Writable NUL-terminated command line (caller-owned, kernel may modify while parsing); NUL-terminated env/cwd slices alive across the call; live attr list; stack structs with exact sizes; NULL app name and no inheritance documented valid | Keep    |
| 33  | `:595` (`spawn_conpty`, unreachable cleanup) | `OwnedHandle::from_raw_handle` (thread)       | Practically unreachable (success fills both handles); wraps whatever is valid so nothing leaks, drops at once                                                                                                                                | Keep    |
| 34  | `:599` (`spawn_conpty`, unreachable cleanup) | `OwnedHandle::from_raw_handle` (process)      | Same fail-closed argument as #33                                                                                                                                                                                                             | Keep    |
| 35  | `:608` (`spawn_conpty`)                      | `OwnedHandle::from_raw_handle` (thread)       | Success plus NULL checks above: fresh, valid, exclusively owned; closed at once                                                                                                                                                              | Keep    |
| 36  | `:609` (`spawn_conpty`)                      | `OwnedHandle::from_raw_handle` (process)      | Same ownership argument as #35; closed once by the returned owner                                                                                                                                                                            | Keep    |
| 37  | `:622` (`terminate_handle`)                  | `TerminateProcess`                            | Borrowed live full-access handle from `CreateProcessW` (includes `PROCESS_TERMINATE`); no pid lookup, so no pid-reuse window                                                                                                                 | Keep    |
| 38  | `:630` (`wait_for_exit`)                     | `WaitForSingleObject(h, INFINITE)`            | Borrowed live handle with `SYNCHRONIZE`; the wait ends when the process exits or another thread terminates it                                                                                                                                | Keep    |
| 39  | `:639` (`wait_for_exit`)                     | `GetExitCodeProcess`                          | Same handle (full access includes query rights); stack `u32` read only after the wait above signaled                                                                                                                                         | Keep    |

Re-verified unchanged: blocks #3–#20 keep their rationales; `resume_threads`
(#15–#20) is still used by the `CREATE_SUSPENDED` pipe-job path.

### Boundary properties (spawn path)

- Inputs beyond pids/handles are caller-built UTF-16 buffers (command line,
  environment block, working directory). They are owned `Vec<u16>`s that
  outlive the `CreateProcessW` call, NUL-terminated by pure builders in the
  safe `conpty` module that refuse NUL (`InvalidInput`), `=` in env keys, and
  empty keys/cwd up front. Lengths are never passed (the kernel finds the
  terminators), so no length can be wrong.
- Console dimensions are validated into `i16` (`COORD` lanes) instead of
  truncating; the attribute list is fixed at two entries (pseudo-console
  plus exactly one job — nesting a second job is not expressible).
- The `PseudoConsole` and `AttributeList` RAII types own their non-`CloseHandle`
  cleanup and never expose a raw `HANDLE`: `HPCON` stays inside `ffi.rs`
  (it is `isize`, not a `HANDLE`, and `CloseHandle` on it would leak).
- Upper layers consume only safe APIs: `bitty-pty` (`forbid`) sees
  `ConPtyMaster`/`ConPtyChild`/`ChildSpec`/`JobObject`/`JobMember` (all
  `OwnedHandle`-owning), and `bitty-runtime` (`forbid`) sees only
  `Pty::tree()` plus `OwnedTree::signal`. `ffi` is crate-private.
- Pure builders (`append_quoted` ported from `portable-pty` 0.9 via
  rust-subprocess `ArgvQuote`, environment block, cwd) carry Windows-only
  unit tests in `conpty.rs` (quoting, double-NUL termination, every
  refusal); they run on the Windows CI job.

### Fixes applied in this task

1. `bitty-winjob`: new safe `conpty` module (`ChildSpec`, `ConPtyMaster`,
   `ConPtyChild`) plus the `ffi` ConPTY section above; `JobObject::as_handle`
   (crate-private borrow for the job list) and `JobMember::from_spawned`
   (crate-private wrap of an already-open handle); `windows-sys` features
   `Win32_System_Console` + `Win32_System_Pipes` (no new crates).
2. `bitty-pty`: Windows backend spawns natively with the job at creation and
   returns the tree with the session; `Pty` owns `tree: Option<OwnedTree>`
   with a `tree()` accessor (Unix adopts the session leader right after the
   spawn — its group is fixed at `fork`, so no window there either);
   `portable-pty` is now a `cfg(unix)` dependency. Removed the
   residual-race notes in `tree/windows.rs` and `tree.rs`; `adopt` is now
   documented for already-running non-PTY children only.
3. `bitty-runtime` `kill_pane_tree`: signals the session's stored tree
   instead of adopting the pid at kill time (the old call site owned the
   whole-lifetime race on Windows).
4. Tests: replaced the adopt-after-start ConPTY test with
   `a_conpty_child_travels_with_its_tree` plus the issue's acceptance test
   `an_immediate_fork_conpty_grandchild_dies_with_the_tree` (helper forks
   first thing under ConPTY; the kill must take the grandchild).
   Windows-CI-only (`require_pty!`); compile-verified here via
   `cargo check --target x86_64-pc-windows-gnu`.

### New residual gaps (follow-ups, NOT fixed here)

- G-5 Registry environment merge: `portable-pty` 0.9 merged machine-level
  registry entries (`SystemRoot`) into the child env; the native backend
  builds the block from the session environment only. A stripped launch
  environment missing `SystemRoot` now fails its spawn fail-closed instead
  of gaining machine state. Whether to restore a targeted allowlist merge
  is a follow-up decision, not a silent fix.
- G-6 Native-spawn runtime proof: no Windows seat on the implementing host;
  `spawn_windows.rs` (env/echo/resize/exit) plus the two job tests above
  are the Windows CI proof. The `cargo check`/`clippy --target
x86_64-pc-windows-gnu` runs here are compile-only evidence.
- G-7 `CreateProcessW` search order: a NULL application name searches the
  application directory, system directories, and `PATH` (with `PATHEXT`),
  slightly broader than `portable-pty`'s pre-resolution; an explicitly set
  but nonexistent `cwd` fails instead of falling back to home. Both are
  fail-closed divergences, documented in `platform/windows.rs`.
- G-1..G-4 carry over unchanged.

### Verification (CTX-0978)

- `cargo test -p bitty-pty -p bitty-winjob -p bitty-runtime` (Linux host)
- `cargo clippy -p bitty-winjob -p bitty-pty -p bitty-runtime --all-targets --locked -- -D warnings` (Linux host)
- `cargo fmt --check -p bitty-winjob -p bitty-pty -p bitty-runtime`
- `cargo check -p bitty-winjob -p bitty-pty --target x86_64-pc-windows-gnu --all-targets` (compile-only; Windows runtime is CI)
- `cargo clippy -p bitty-winjob -p bitty-pty --target x86_64-pc-windows-gnu --all-targets --locked -- -D warnings`
- Negative control: `rg 'unsafe \{|unsafe fn|unsafe impl|allow\(unsafe_code\)' crates/*/src`
  matches only the allowlisted `gpu.rs` pair, the `bitty-render` and
  `bitty-winjob` module allowances, and the 37 `bitty-winjob/src/ffi.rs`
  blocks inventoried above.
