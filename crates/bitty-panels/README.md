# `bitty-panels`

> Part of the `bitty` workspace. Canonical product and architecture
> documentation lives in `bitty-terminal-docs` (mounted at `docs/`) and
> shared governance in `bitty-docs`; this file is a crate-local map, not a
> canonical contract.

## Purpose

`bitty-panels` is the staging home for first-party panel experience
implementations that the `CTX-0438` extraction wave moved out of the
`bitty-runtime` microkernel crate (024 §17.3; 2026-09-16 architectural-drift
review §4). It owns optional experience policy — capability strings, bounds,
pure listing/filtering/validation helpers, and tiled-layout assembly — while
`bitty-runtime` keeps terminal mechanism.

The crate consumes only the accepted public Panel Runtime path:
`bitty_runtime::registry` (`PanelRegistry`, event bus, typed errors) plus
`bitty_ui` layout primitives. There is no private channel, no first-party
bypass, no parser/renderer/input hot path, and no grid mutation (only `Action`
writes `State`).

## Status

Staging boundary, per the accepted OQ-053 verdicts (see `src/lib.rs` for the
full statement): the plugin splits remain gated on the panel-provider contract
(bitty-docs `CTX-0181`, OQ-058) and the per-panel gates.

- `ai_panel` — split later (hybrid); gates: panel-provider contract and
  `bitty-ai` surfaces; owning task `CTX-0402`.
- `mail_panel` — split later; gates: panel-provider contract and
  credential-source contract (OQ-054/OQ-055); owning task `CTX-0403`.

Until the gates land, this crate changes no bundled catalog entry, manifest,
capability string, or wire shape. The bundled manifests stay in
`bitty-plugin-host::bundled`. Panels recorded as Core or already split
(`browser-panel` `CTX-0401`; `palette`/`statusline` residual Core helpers
`CTX-0397`/`CTX-0398`; `project`, `shell-integration`, and the workspace core)
deliberately stay in `bitty-runtime` in this phase.

## Boundaries

- Workspace-internal dependencies only, per `Cargo.toml`: `bitty-agent`,
  `bitty-ipc`, `bitty-runtime`, and `bitty-ui`; no network-facing dependency
  is declared.
- No Lua, config, or plugin host code enters the hot path.
- Default CI verifies the headless seam only.

## Layout

- `Cargo.toml` — package metadata and workspace-internal dependencies.
- `src/lib.rs` — crate docs with the extraction-wave boundary and status.
- `src/scaffold.rs` — private generic panel-session scaffolding
  (`create_panel` → `mount_panel`, registry-config validation) shared by the
  staged modules.
- `src/ai_panel.rs` — `bitty-terminal.ai-panel` implementation and state.
- `src/mail_panel.rs` — `bitty-terminal.mail-panel` implementation and state.
- `tests/` — headless public-path verification moved with the modules.
