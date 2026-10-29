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
  `<data_home>/bitty/extensions/<name>/current` and the version's
  `bitty-component.toml` (developer override `BITTY_COMPONENTS_DIR`; never
  `PATH`), validates the descriptor and the executable's SHA-256 before
  every spawn (fail closed), runs the component as a stdio coprocess with a
  cleared environment plus a fixed allowlist, captures stderr into a 64 KiB
  ring, completes the `Hello`/`HelloAck` handshake, multiplexes up to 64
  requests by id, stops the process after 60 s idle (stdin close, 2 s
  grace, then a kill of the recorded child only), and turns a crash into
  `component_lost` for in-flight requests with a 1 s to 30 s restart
  backoff (5 crashes in 5 minutes make it unavailable until restart). Wire
  protocol selection is per component family (`src/component/family.rs`):
  `graphics`, `net`, `storage`, and `a11y` use the shared v1 wire, the
  `execution` family is hosted by `src/execution_component/` on the
  `bitty-execution` v2 codec, and unlisted names keep the strict v1 default.
  Every request carries the plugin id and the grant computed from the granted
  `network.connect:*` capabilities intersected with the manifest
  `[[network.egress]]` entries. The Lua-facing request surface and process
  sandboxing are follow-ups; nothing calls the broker from Lua yet. The
  `bitty-component-fixture` binary is a hermetic test fixture only (never
  packaged).
- Resident AI helper (`src/ai_helper.rs`, DIR-030 S-1, bitty#2123): the
  Core-side spawn path for the native `ai` component (executable `bitty-ai`).
  It reuses the `src/component/` resolution (`resolve_search`, `PATH` never
  read) and per-spawn SHA-256 digest verification, then spawns the helper as
  a **resident** stdio coprocess over the accepted bitty-ipc length-prefixed
  framing. Unlike `net` it is never idle-stopped; it keeps one fenced writer
  (single stdin owner per generation), a bounded inbound frame queue, a 64
  KiB stderr ring, and the DIR-030 restart bounds (1 s→30 s backoff, 5
  crashes in 5 minutes latch it unavailable until a new session). The `ai.*`
  handler surface, the snapshot-version correlator, and the per-request gates
  stay helper-side; the Lua/IPC surface that drives `submit_frame` and
  registry install sources are follow-ups. `bitty-ai-helper-fixture` is a
  hermetic test fixture only (never packaged).
- Core AI request producer (`src/ai_request.rs`, AI-0314 / bitty#2162): owns a
  bounded metadata snapshot with monotonic saturating version bumps and builds
  every supported outbound `ai.*` envelope through one path. Its public
  metadata boundary accepts bounded ASCII identifiers for the initial
  service/version/capability slice, including legitimate names such as
  `token-counting`. It rejects only credential-shaped values such as URL
  userinfo, bearer/API-key assignments, and known key/JWT forms; no credential
  or network access is performed. Provider/model names and routing facts are
  deferred behind a typed extension point rather than overloaded into service
  strings.
  It rejects malformed, duplicate, caller-supplied correlator, over-depth, and
  over-bound input before calling `ResidentHelper::submit_frame`; unknown
  fields remain helper-owned. Metadata mutation, refresh, and invalidation
  explicitly stale the last acknowledgement. Requests fail closed until the
  current immutable publication is acknowledged through the explicit local
  Core seam. The
  producer is a tested Core seam only: application event-loop and Lua/IPC
  callers, automatic metadata hooks, and a helper metadata endpoint are not
  present in this repository yet. The follow-up integration must transport
  `publish_metadata()` to that boundary. The current
  `acknowledge_local_metadata()` method is explicitly a local test/integration
  seam and does not prove helper acceptance; a future helper adapter must own
  that receipt before this code can claim end-to-end synchronization.
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

## Session attach/detach seam (bitty#2729, CTX-1655)

`src/attach.rs` is the Core half of the accepted
[Multiplexer Decision](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/multiplexer-decision.md)
(Option C). It is a bounded **binding-lifecycle** seam over the accepted
[`bittyd` Session-Host Contract](https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md)
(`D-09`, `D-10`, `D-14`): a Core consumer binds a window and panel to a session
view, receives metadata-only lease events routed to that binding, and releases
it again. It adds no IPC method, no `session.*` scope, and no transport.

| Layer                            | Owns                                                                                                                                                                                               | Does not own                                                                                              |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Core seam (`attach::AttachSeam`) | Local binding lifecycle, one binding per `(window, session)`, the per-window cap, idempotent attach/detach, per-lease event routing, release on window close, session termination, and daemon stop | Session, lease, attach-count, control, or last-seen truth; terminal content; naming or persistence policy |
| `bittyd`                         | Session registry, leases, control and observer roles, attach count, generations, last-seen, and every `session.attach`/`session.detach` decision; it is the sole writer                            | Window/panel binding state, extension UI, persistence policy                                              |
| Extension (bitty#2730)           | Session naming, ordering, organization, persistence timing, restore policy, and the session UI                                                                                                     | The Core binding table, the lease, the PTY, or the snapshot codec                                         |
| Remote                           | Nothing yet: ADR 0008 and ADR 0019 defer remote attach to a separate accepted trust-boundary ADR                                                                                                   | Any local scope or transport                                                                              |

`attach::SessionHost` is the transport-agnostic port the future consumer-side
`bittyd` client implements; this tree has no such adapter yet, so the slice
does not claim an end-to-end attachment. The seam never holds a control token,
observer grant, PTY descriptor, grid, scrollback, or terminal text: credentials
are borrowed for one call, and `SessionEvent` is a fixed-size metadata record.
A control-transfer frame is delivered as metadata, but this lifecycle slice
does not store the rotated control token or change the binding role; a future
Core control path must re-claim through the daemon client.

Bounds: one live binding per `(window, session)`, at most
`MAX_BINDINGS_PER_WINDOW` (16) per window with an 8 default, and one bounded
queue per binding (256 events / 64 KiB). Every v0.2.0 frame is a control frame,
so a full queue closes the lease with `TerminalEventLost` rather than shedding
it. A failed or refused detach still removes the local binding; the refusal is
returned in the release receipt, so no local lease record leaks.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs with the data flow and headless seam.
- `src/ai_helper.rs` — resident-singleton supervisor for the native `ai`
  helper (`bitty-ai`): DIR-030 resolution plus SHA-256 digest verification,
  stdio-coprocess spawn over the accepted bitty-ipc framing, and the bounded
  restart policy (DIR-030 S-1, bitty#2123). Contract record:
  `specifications/ai-helper-resident-singleton.md`.
- `src/runtime.rs` and `src/runtime/` — runtime handle plus resize, input,
  panes, present, plugin, search, selection, and workspace slices;
  `src/runtime/band_slots.rs` is the single host placement policy for the v1
  `bitty.ui.mount` slots (shared by the mount gate and chrome band routing).
- `src/queue.rs` — bounded cold-path event queue.
- `src/attach.rs` — bounded Core attach/detach binding seam for daemon-hosted
  sessions (bitty#2729): per-window binding table, idempotent attach/detach,
  metadata-only per-lease event routing, and release on window close, session
  termination, or daemon stop. Host port for the future consumer-side
  `bittyd` client; no IPC method or scope is added here.
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
