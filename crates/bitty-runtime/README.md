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

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-vt`,
  `bitty-term-state`, `bitty-pty`, `bitty-render`, `bitty-platform`,
  `bitty-ui`, `bitty-lua`, `bitty-plugin-host`, `bitty-package`, and
  `bitty-rich`; external git dependency: `bitty-ipc` (independent
  repository since CTX-1585), linked as Core's inbound local socket
  mechanism. Core links no agent crate: `bitty-agent` is an independent
  repository with its own tests (CTX-0918). Core links no network code:
  the only crate taken from the bitty-network repository is
  `bitty-network-wire` (exact-rev git pin), the dependency-free wire
  protocol v1 codec the component broker speaks (CTX-0906, DIR-030).
- Native components (`src/component/`): the broker resolves
  `<data_home>/bitty/components/<name>/current` and the version's
  `bitty-component.toml` (developer override `BITTY_COMPONENTS_DIR`; never
  `PATH`), validates the descriptor and the executable's SHA-256 before
  every spawn (fail closed), runs the component as a stdio coprocess with a
  cleared environment plus a fixed allowlist, captures stderr into a 64 KiB
  ring, completes the `Hello`/`HelloAck` handshake, multiplexes up to 64
  requests by id, stops the process after 60 s idle (stdin close, 2 s
  grace, then a kill of the recorded child only), and turns a crash into
  `component_lost` for in-flight requests with a 1 s to 30 s restart
  backoff (5 crashes in 5 minutes make it unavailable until restart). Every
  request carries the plugin id and the grant computed from the granted
  `network.connect:*` capabilities intersected with the manifest
  `[[network.egress]]` entries. The Lua-facing request surface and process
  sandboxing are follow-ups; nothing calls the broker from Lua yet. The
  `bitty-component-fixture` binary is a hermetic test fixture only (never
  packaged).
- No Lua, config, or plugin code enters the hot path (see `src/lib.rs`).
- The plugin side queue never holds hot-path objects: no GPU, window, or PTY
  handles and no Lua VM.
- Default CI verifies the headless software seam only; attaching a real GPU
  surface awaits a follow-up slice and must not be described as implemented.
- Optional experience policy does not live here: the first-party `ai-panel`
  and `mail-panel` implementations left this crate in the `CTX-0438`
  extraction wave, and the emptied `bitty-panels` staging crate was retired
  by CTX-0918. This crate keeps the generic Panel Runtime mechanism
  (`registry/`) plus the recorded Core slices above.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs with the data flow and headless seam.
- `src/runtime.rs` and `src/runtime/` — runtime handle plus resize, input,
  panes, present, plugin, search, selection, and workspace slices.
- `src/queue.rs` — bounded cold-path event queue.
- `src/registry.rs` and `src/registry/` — command and panel registries.
- `src/execution.rs` and `src/execution/` — execution supervisor
  (`CTX-0511` foundation + `CTX-0513` delivery + `CTX-0514` capability-scoped
  operations): AI-agnostic async job registry, job model, bounded per-job
  output store with tail/filter reads, critical/observation event delivery
  with reconnect replay, and per-principal per-operation grants
  (observe/read_output/write_input/signal/cancel/attach/transfer) enforced
  in-process with deny-by-default above the PTY/process primitives.
- `src/plugin_runtime/` — plugin runtime wiring and services.
- `src/config.rs` — runtime-side configuration application.
- `src/workspace.rs`, `src/tabs.rs` — workspace and tab orchestration.
- `src/paste.rs`, `src/inspect.rs` — paste confirmation gate and bounded
  read-only inspection snapshots. The OSC 7/133 read view used by the IPC
  snapshot is `bitty_rich::shell::ShellIntegration`; the residual bundled
  palette/statusline panel helpers were removed in `CTX-0922` (they ship as
  Lua plugins after the OQ-053 split).
- `src/error.rs` — owned error types.
- `tests/` — headless orchestration tests.
