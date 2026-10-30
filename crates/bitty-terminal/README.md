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
- Opt-in cargo feature `dev-tools` (off by default, CTX-0922) compiles
  `bitty dev capture|synthesize|dump|overlay`. Without it the verbs still
  validate their arguments and then fail with exit 1 and
  `bitty dev <verb>: built without dev-tools feature`.
- `bitty dev trace startup|latency` measurement lives in the external
  `bitty-perf` validation suite (W-105 relocation) and is never linked here;
  the verb still validates its arguments and then fails with exit 1.
- Third-party dependencies, per `Cargo.toml`: `pollster` only; no
  network-facing dependency is declared.
- The only network-reaching path is the pinned extension install seed
  (`src/component_seed.rs`, bitty#2399): fixed-argv system `curl`, one
  `(name, version, host target)` release plus its `SHA256SUMS`, verified
  before staging. Core does not fetch or parse `extensions/catalog.v1`;
  catalog resolution, latest selection, and list/inspect are
  `bitty-plugin-manager` work (`W-72`). See
  `specifications/extension-install-seed.md` for the bounds, offline
  fallback, and trust chain.
- Must not own business behavior: grid, parsing, rendering, plugin, and IPC
  semantics belong to the libraries it wires.
- Flag parsing itself is pure and total; config-file loading, window and GPU
  attachment stay on the documented startup path in `src/main.rs`.

## Observability boundary (CTX-0926, W-100 first slice)

> Contract basis: accepted W-71 `bitty-docs/docs/development/observability-boundary.md`.
> This crate keeps the first staged slice only: the boundary is defined and
> behavior is preserved — no debug or trace code is retired here (removal
> needs the W-71 gates plus `W-110` conformance first).

- Retained always-on Core mechanism (compiled in, safe-mode clean):
  the stderr verbosity gate in `src/logging.rs` (quiet `Warn` default;
  per-frame `bitty tick` lines need `--verbose` / `--log-level debug|trace`
  or `BITTY_LOG`/`RUST_LOG`), the bounded read-only inspect snapshots
  served from `bitty-runtime`, and the default-deny gate plus
  redaction-at-emission plus bounds in `src/observability.rs`
  (every `logging::info`/`warn` line is scrubbed before `eprintln!`;
  `secret://` handles stay log-safe).
- Optional policy (explicit opt-in, default off, safe-mode clean):
  `bitty dev trace` (measured by the external `bitty-perf` validation suite
  since the W-105 relocation, never linked into this binary),
  `bitty dev capture|synthesize|dump|overlay` (needs the `dev-tools`
  feature), the tick-line verbosity flags above, `BITTY_DEMO_PUMP=1`
  (suppressed under `--safe` with one explicit warning), and
  `BITTY_PERF_STARTUP_MARKER`. The `bitty-observability` implementation,
  exporters, and metrics pipeline are not Core dependencies.
- Transition: `bitty-runtime` `plugin_runtime::debug` (`DebugView`/`TraceHub`)
  is staged as future `bitty-observability` implementation but stays compiled
  in until the removal gates pass; `plugin_runtime::redaction` stays in Core
  permanently. No Event-Bus exposure of observations, no secret capture,
  version negotiation fails closed (pre-`0.1.0` makes no stability claim).

## Plugin inspect surface (CTX-1634, issue #2773)

- `bitty.debug/listPlugins` and `bitty.debug/getPlugin` are served from a
  process-wide store that `src/main.rs` publishes once, after the plugin
  runtime is attached, from the runtime's sanitized `DebugView` rows. The copy
  is a **one-shot startup snapshot**: the runtime keeps its own view current
  after every lifecycle transition, but later lazy activation, suspension,
  disposal, or failure is not re-published here, so the wire row can lag the
  runtime state. Publishing from the runtime (a transition hook) is the
  follow-up that would remove the staleness.
- Registration is not test-mode-only: `build_dispatcher` installs the two
  handlers on both the test-mode and the default table. The rows stay behind
  the existing elevated `debug.inspect` / `debug.trace` / `debug.control`
  scope check, a runtime-less run keeps the typed
  `PluginRuntimeUnavailable` verdict, and no trace, control, VM, store,
  settings, or terminal read is added.
- Rows are normalized to the accepted `PluginSummary` guards before they are
  stored: only addressable generations (`generation >= 1`) are served, at most
  256 rows, at most 256 `capabilities` entries per row, and at most 128 bytes
  per capability id. Declared capability ids can be longer than that bound
  (for example a long `fs.read:<pattern>`), so unrepresentable ids are omitted
  rather than truncated; the row stays addressable and the authoritative grant
  set is served by the grant surfaces, never by this metadata array.

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
