# `bitty-lua`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-lua` wraps the pure-Rust `phodopus` VM (sole third-party dependency
per `Cargo.toml`, exact git revision `1653c51f7fbda5e93fa99aefb0e5be58dfacfeb0`)
to give Bitty deterministic, bounded Lua execution for per-plugin isolation
and, per DEC-0011, user configuration evaluation (`src/lib.rs`,
`src/config.rs`). Each VM instance gets isolated globals with a restricted
standard library (`src/stdlib.rs`), host work happens only through
capability-checked callbacks (`src/host.rs`), and instruction, wall-clock,
and heap budgets fail closed into suspension (`src/lib.rs`: `drive_chunk`
and `drive_stashed`, sliced at `SLICE_FUEL = 1024`). Budget mechanics and
the determinism contract are documented in `src/lib.rs`.

## Boundaries

- Sole third-party dependency, per `Cargo.toml`: `phodopus` pinned at exact
  git revision `1653c51f7fbda5e93fa99aefb0e5be58dfacfeb0`; no
  network-facing dependency is declared.
- Standard library baseline at `phodopus@1653c51f`: in-core `utf8` and
  `string.format` with proportional fuel charging, plus retained bounded
  `string.byte`/`char`, `table.concat`/`sort`, and restricted
  `os.time`/`clock`/`date` (`src/stdlib.rs`); no ambient `io` authority,
  traceback-only `debug`, text-only `load`, and empty `package.path` with no
  native loader (pinned by `tests/readiness_mirror.rs`:
  `stdlib_allowlist_denies_ambient_authority`).
- Plugin VMs build only through the fail-closed gate (`src/gate.rs`:
  `PluginVmBuilder`/`build_plugin_vm` require explicit RC-1/RC-2 budgets);
  the raw constructors are deprecated and kept for the single internal
  gate path (pinned by `tests/load_gate.rs`).
- Configuration chunks run under the same budgets as plugins and must return
  plain-data tables; the typed schema in `bitty-config` stays the validation
  authority (see `src/config.rs`).
- There is no `mlua` dependency anywhere, so no VM type conflict can arise
  (`src/lib.rs` documents the `phodopus::Lua` boundary).

## Evidence

- `tests/readiness_mirror.rs` (RUN-29) mirrors the Phodopus executable gate
  (`phodopus@1653c51f`, `crates/phodopus/tests/readiness_gate.rs`) at the
  bitty seam: RC-1 instruction/wall budgets, RC-2 quota refusal, FS-1
  denial atomicity, FS-3 fault containment, FS-5 recovery, FS-9 no-bypass,
  the stdlib allowlist, module isolation, and host-op cancellation as typed
  `E_TIMEOUT`; every test fails if the mapped behavior regresses and no
  test asserts on wall-clock durations.
- `tests/measurement_lua.rs` proves the RC-1/RC-2 enforcement curves and
  the `VmBudgetSnapshot` counter methodology; `tests/host_bridge.rs` proves
  the bridge deadline/timeout paths; `tests/load_gate.rs` proves the
  fail-closed build gate and stable `E_*` budget codes; `tests/safe_mode.rs`
  proves `--safe` admits zero third-party surface.

## Layout

- `Cargo.toml` — package metadata and the `phodopus` dependency.
- `src/lib.rs` — crate docs with the role, budgets, and determinism sections.
- `src/host.rs` — VM lifecycle, budget enforcement, and host calls.
- `src/ui.rs` — Plugin API v1 declarative UI scenes (`bitty.ui.mount` /
  `bitty.ui.update`) and the bounded `UiNode` model.
- `src/config.rs` — configuration-chunk evaluation and table extraction.
- `src/stdlib.rs` — restricted standard-library construction.
- `src/gate.rs` — fail-closed plugin-VM build gate and safe-mode policy.
- `src/error.rs` — stable `E_*` bridge codes for VM budget failures.
- `tests/` — headless budget and isolation tests, including the
  `readiness_mirror.rs` gate mirror.
