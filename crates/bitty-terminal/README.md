# `bitty-terminal`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-terminal` is the `bitty` binary: the thin composition root that parses
arguments, loads user configuration through the `bitty-lua` sandbox via
`bitty-config`, creates the `bitty-runtime` runtime, wires layout and focus,
spawns the shell through the PTY layer, and forwards platform events into
`tick`-to-present. It owns no business logic beyond wiring already-owned
libraries, as stated in `src/main.rs`.

## Boundaries

- Workspace-internal dependencies, per `Cargo.toml`: `bitty-config`,
  `bitty-platform`, `bitty-plugin-host`, `bitty-render`, `bitty-rich`,
  `bitty-runtime`, `bitty-term-state`, and `bitty-vt`; external git
  dependency: `bitty-ipc` (independent repository since CTX-1585), linked as
  Core's inbound local socket mechanism. No agent crate is linked.
- Opt-in cargo feature `dev-perf` (off by default, CTX-0918) adds the
  `bitty-perf` dependency for `bitty dev trace startup|latency`. Without it
  the verb still validates its arguments and then fails with exit 1 and
  `built without dev-perf feature`.
- Opt-in cargo feature `dev-tools` (off by default, CTX-0922) compiles
  `bitty dev capture|synthesize|dump|overlay`. Without it the verbs still
  validate their arguments and then fail with exit 1 and
  `bitty dev <verb>: built without dev-tools feature`.
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
