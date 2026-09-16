# `bitty-runtime`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-runtime` is the Correct Terminal orchestration crate: it owns the
lifecycle of the PTY, VT parser, terminal state, grid renderer, and surface,
and exposes a narrow owned API that never leaks upstream types. The hot path
runs PTY bytes through parsing, state, damage, and render to present, while
cold-path events flow through a bounded queue into the plugin host side queue
and multi-pane layout with focus reflows deterministically. The data-flow
diagram lives in `src/lib.rs`.

## Boundaries

- Workspace-internal dependencies only, per `Cargo.toml`: `bitty-vt`,
  `bitty-term-state`, `bitty-pty`, `bitty-render`, `bitty-platform`,
  `bitty-ui`, `bitty-lua`, `bitty-plugin-host`, `bitty-agent`, `bitty-ipc`,
  `bitty-package`, and `bitty-rich`; no network-facing dependency is
  declared.
- No Lua, config, or plugin code enters the hot path (see `src/lib.rs`).
- The plugin side queue never holds hot-path objects: no GPU, window, or PTY
  handles and no Lua VM.
- Default CI verifies the headless software seam only; attaching a real GPU
  surface awaits a follow-up slice and must not be described as implemented.
- Optional experience policy does not live here: the first-party `ai-panel`
  and `mail-panel` implementations moved to the `bitty-panels` staging crate
  in the `CTX-0438` extraction wave. This crate keeps the generic Panel
  Runtime mechanism (`registry/`) plus the recorded Core slices above.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs with the data flow and headless seam.
- `src/runtime.rs` and `src/runtime/` — runtime handle plus resize, input,
  panes, present, plugin, search, selection, and workspace slices.
- `src/queue.rs` — bounded cold-path event queue.
- `src/registry.rs` and `src/registry/` — command and panel registries.
- `src/plugin_runtime/` — plugin runtime wiring and services.
- `src/config.rs` — runtime-side configuration application.
- `src/workspace.rs`, `src/tabs.rs` — workspace and tab orchestration.
- `src/panels_async.rs`, `src/browser_panel.rs` — generic panel worker and
  the stay-bundled browser-panel Core slice (`CTX-0401`).
- `src/palette.rs`, `src/statusline.rs`, `src/paste.rs`,
  `src/shell_integration.rs`, `src/project.rs`, `src/inspect.rs` —
  experience helpers; `palette`/`statusline` are the residual Core Panel
  Runtime helpers recorded by the `CTX-0397`/`CTX-0398` splits.
- `src/error.rs` — owned error types.
- `tests/` — headless orchestration tests.
