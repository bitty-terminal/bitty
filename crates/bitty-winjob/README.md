# `bitty-winjob`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-winjob` is the narrow, reviewed Win32 Job Object adapter behind the
Windows owned-process-tree backend in `bitty-pty` (CTX-0903, DEC-0083). It
creates kill-on-close Job Objects, assigns processes, terminates whole
jobs, observes member exit without reaping, and resumes children created
suspended. See `src/lib.rs` for the public surface.

## Boundaries

- Every `unsafe` block lives in the private `src/ffi.rs` module, each with a
  `SAFETY` rationale, and is inventoried in
  `specifications/unsafe-ffi-audit.md`; the crate root denies
  `unsafe_code`.
- No raw handle or pointer crosses the public API; handles are owned
  `OwnedHandle` values.
- Sole dependency, Windows only: `windows-sys` (workspace pin). On every
  other platform the crate is empty and dependency-free.

## Layout

- `Cargo.toml` — package metadata and the Windows-only `windows-sys`
  dependency with its feature list.
- `src/lib.rs` — crate docs, lint policy, and re-exports.
- `src/job.rs` — safe `JobObject`/`JobMember` API and Windows unit tests.
- `src/ffi.rs` — the audited Win32 calls.
