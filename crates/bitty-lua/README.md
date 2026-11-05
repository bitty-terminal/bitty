# `bitty-lua`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-lua` wraps the pure-Rust `phodopus` VM (sole third-party dependency
per `Cargo.toml`, exact git revision `f6aa59a16c0dd8d3f65f54043f8c3c2fe02b5344`)
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
  git revision `f6aa59a16c0dd8d3f65f54043f8c3c2fe02b5344`; no
  network-facing dependency is declared.
- Standard library baseline at `phodopus@f6aa59a`: in-core `utf8` and
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

## Lua surface: presentation overlay (issue #2849)

```lua
local handle, err = bitty.ui.mount("overlay", scene)   -- needs ui.rich + ui.overlay
local shown, err = bitty.ui.present.show(handle)        -- needs ui.present
local hidden, err = bitty.ui.present.hide(handle)       -- idempotent, ungated
```

The surface is non-modal and paint-only: it draws above the focused view,
never receives key or pointer events, never takes focus, and never changes the
focus ring. Core owns geometry, clipping, stacking, the frame budget, and
teardown; the plugin owns only the scene.

| Item         | Contract                                                                                                                                                                                                                                                                                                 |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `show`       | Argument: an integer block handle this generation mounted into the `overlay` slot. Returns `true` on success and `false` only where documented by the host.                                                                                                                                              |
| `hide`       | Argument: the same handle. Idempotent: `false` means "nothing of mine was live", never an error. Cleanup stays ungated so a revoked grant can still remove the surface.                                                                                                                                  |
| Grants       | `ui.present` alone authorizes drawing. `ui.rich` + `ui.overlay` are needed to mount the block; `ui.overlay.focus` is required only for the focusable capture overlay and is neither implied by nor implied from `ui.present`.                                                                            |
| Admission    | One surface per window per generation; a second plugin's `show` fails closed with `E_UI_UNAVAILABLE`. Never sets the modal authority.                                                                                                                                                                    |
| Budget       | Bounded like a chrome band: at most 64 rows and 16 KiB of row text per frame, plus the v1 scene budgets enforced at mount. The trailing rows are dropped and the drop is reported, never silently painted.                                                                                               |
| Focus-follow | The surface tracks the focused view. A focus switch re-centers it; closing the view it was drawn over removes it.                                                                                                                                                                                        |
| Dismissal    | `Esc` dismisses it without consuming the press (a real confirmation gate keeps its consume). The owner observes one `overlay.dismissed` event with `{ owner, handle, reason }` where `reason` is `"escape"` or `"view-closed"`; delivery is queued to the cold tick, never a callback on the input path. |
| Errors       | `E_CAPABILITY_DENIED` (missing `ui.present`), `E_UI_UNAVAILABLE` (safe mode, no host surface, or a second plugin's admission), `E_UI_NOT_OWNER` (a handle this generation did not mount into the `overlay` slot), `E_UI_COMPONENT_INVALID` (a non-integer handle), `E_TIMEOUT` (call deadline).          |
| Lifecycle    | Suspend, unload, disable, crash, rollback, and generation change drop the surface and release its admitted slot; a stale generation's next present paints nothing.                                                                                                                                       |
| Safe mode    | No plugin VM exists, so no surface exists. A generation that reaches the host under safe policy is refused with `E_UI_UNAVAILABLE`.                                                                                                                                                                      |

`bitty.ui.overlay.show` and `bitty.ui.overlay.hide` remain as the CTX-1431
spelling of the same session; they are gated by `ui.present` as well, so
neither spelling can gain input authority. The focusable capture surface
(`bitty.ui.overlay.acquire`, `update`, `poll`, `release`) and its
`E_UI_ALREADY_CAPTURED` single-owner contract are unchanged.

## Lua surface: `process.spawn` working directory (CTX-1715)

```lua
local result = bitty.process.spawn(
  { "status", "--porcelain" },
  { cwd = "pane" }  -- optional; "pane" is the only accepted value
)
```

`cwd` selects a **semantic**, never a path. `"pane"` asks the host to run the
child in the directory of the terminal pane the calling plugin is bound to
(the host-observed focused pane, the same live model behind
`bitty.terminal.snapshot`). The host validates the resolved directory
(absolute, at most 4096 bytes, no NUL or control bytes, canonicalized, an
existing directory) and records the resolved canonical path in the plugin
spawn audit entry. A plugin can never supply, override, or influence that
path.

| Item                      | Contract                                                                                                                                                                                                                        |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cwd`                     | Optional string. `"pane"` is the only accepted value. An absolute or relative path, `~`, `..`, a symlink target, an empty string, and a wrong-case token are all `E_DEF_INVALID` before the host call.                          |
| Unknown options           | `E_DEF_INVALID`; no option is ever silently ignored. A table mixing `cwd = "pane"` with an unknown key is refused regardless of key order.                                                                                      |
| `E_SPAWN_CWD_UNAVAILABLE` | The host has no pane binding, the bound pane has not reported a directory (`OSC 7`), or the report is unusable. The spawn never falls back to the host process cwd.                                                             |
| Unchanged refusals        | The `-C`, `--git-dir`, and `--work-tree` argv denials and the `[tools.git]` executable allowlist are unchanged; the pane directory selects the repository, argv may not.                                                        |
| Not a filesystem grant    | The pane directory is a **starting directory**, not an `fs` grant. It does not widen what the plugin may read: plugin API reads still follow `fs.read`/`fs.write`, and the child keeps the existing inherited exec confinement. |
| Reload-free               | The source is read per call, so changing the pane directory changes the result without reloading the plugin.                                                                                                                    |
| Unchanged calls           | Omitting the option keeps the host default (no child cwd), so existing calls stay valid.                                                                                                                                        |

## Evidence

- `tests/readiness_mirror.rs` (RUN-29) mirrors the Phodopus executable gate
  (`phodopus@f6aa59a`, `crates/phodopus/tests/readiness_gate.rs`) at the
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
  `bitty.ui.update`), the closed v1 `UiSlot` enum, and the bounded `UiNode`
  model.
- `src/keymaps.rs` — bounded keymap/action registry page for
  `bitty.keymaps.list()` (issue #2405): canonical rows, 256-row pages, the
  1024-row addressable cap, and 64 KiB whole-row tail truncation.
- `src/config.rs` — configuration-chunk evaluation and table extraction.
- `src/stdlib.rs` — restricted standard-library construction.
- `src/gate.rs` — fail-closed plugin-VM build gate and safe-mode policy.
- `src/store.rs` — host-owned RC-11 persistent-store quota backend
  (`bitty.store`; pinned by `tests/store_quota.rs`).
- `src/error.rs` — stable `E_*` bridge codes for VM budget failures.
- `tests/` — headless budget and isolation tests, including the
  `readiness_mirror.rs` gate mirror.
