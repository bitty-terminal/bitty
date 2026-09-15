# `bitty-app`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-app` is the `bitty` binary: the thin composition root that parses
arguments, loads user configuration through the `bitty-lua` sandbox via
`bitty-config`, creates the `bitty-runtime` runtime, wires layout and focus,
spawns the shell through the PTY layer, and forwards platform events into
`tick`-to-present. It owns no business logic beyond wiring already-owned
libraries, as stated in `src/main.rs`.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-config`,
  `bitty-ipc`, `bitty-perf`, `bitty-platform`, `bitty-plugin-host`,
  `bitty-render`, `bitty-runtime`, and `bitty-term-state`.
- Third-party dependencies, per `Cargo.toml`: `pollster` only; no
  network-facing dependency is declared.
- Must not own business behavior: grid, parsing, rendering, plugin, and IPC
  semantics belong to the libraries it wires.
- Flag parsing itself is pure and total; config-file loading, window and GPU
  attachment stay on the documented startup path in `src/main.rs`.

## Layout

- `Cargo.toml` — binary target `bitty` at `src/main.rs` plus dependencies.
- `src/main.rs` — crate docs and the owned startup flow.
- `src/cli.rs` — argument parsing (`--headless`, layout, focus, config flags).
- `src/run.rs` — runtime creation and the event-loop driver.
- `src/spawn.rs` — shell resolution and PTY spawn wiring.
- `src/terminal_app.rs` — terminal application assembly.
- `src/config_cli.rs` — configuration flag handling.
- `src/chrome_keys.rs` — chrome keybinding consumption rules.
- `src/plugin_runtime.rs` — plugin runtime attachment.
- `src/ipc_serve.rs` — IPC serving wiring.
- `src/doctor.rs` — diagnostics wiring.
- `tests/` — integration tests for the composition root.
