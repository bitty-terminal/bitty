---
title: Panel/Tabs Reference Notes — Hyprland/Waybar (CTX-0084)
description: Read-only Hyprland c91fa5a BSD-3 and Waybar 6d60c8e MIT clones with exa dwindle, Waybar module provider, and winit layer-shell patterns for future workspace compositor panel/tabs
category: product
audience: maintainer
document_type: research
status: draft
---

<!-- markdownlint-disable MD025 -->

# Panel/Tabs Reference Notes — Hyprland/Waybar (CTX-0084)

## Status and provenance

- Status: **draft**. Research only — no product code, no dependencies added, no execution.
- Owner: `opencode-commander` (health patrol follow-up, `bitty` repo `main 1ab5fb9` descendant of `91705be`).
- Task: bitty **CTX-0084** — _Hyprland/Waybar reference clone and exa research for panel/tabs_.
  - Priority: P0 | Area: ui | Labels: docs,area:ui,P0 | Milestone: v0.1.0 | RFC: workspace-compositor | Task: CTX-0084
  - Issue: [#120](https://github.com/bitty-terminal/bitty/issues/120) — `docs,area:ui,P0` — milestone `v0.1.0`
- Worktree: `.worktrees/ctx-0084-reference-clone` — branch `ctx-0084/reference-clone` — base `1ab5fb9`.
- Scope: clone Hyprland/Waybar at depth 1 to **global** `tmp/references/` (umbrella, outside `bitty` worktree per isolation), record revision + license in `tmp/references/README.md`, summarize exa patterns in `tmp/references/panel-tabs-research-2026-08-30.md`, and record this docs artifact. No `tmp/references/` commit inside `bitty`; this file is the in-repo artifact.

## Global snapshots — verification

Read-only clones under umbrella `tmp/references/` (never executed, never imported as dependencies, not referenced in `Cargo.toml`):

| Snapshot   | Upstream                             | Revision (short) | Full HEAD                                                                                                                              | License                                                   | Cloned                           |
| ---------- | ------------------------------------ | ---------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- | -------------------------------- |
| `hyprland` | <https://github.com/hyprwm/Hyprland> | `c91fa5a`        | `c91fa5ab4d566206888c708dba66fca3646c382e` — `fullscreen: fix missing early return (#16063)`                                           | BSD-3-Clause — `LICENSE` Copyright (c) 2022-2026 vaxerski | 2026-08-30 `git clone --depth 1` |
| `waybar`   | <https://github.com/Alexays/Waybar>  | `6d60c8e`        | `6d60c8e02be67bb85bb9b1ea803f2fbcf0722002` — `Merge pull request #5222 from IlyasKhallouki/fix/5220-taskbar-dedup-only-with-max-icons` | MIT — `LICENSE` Copyright (c) 2025 Alex                   | 2026-08-30 `git clone --depth 1` |

Retained for VT/grid differential (unchanged): `ghostty@8867c37` MIT, `kitty@087b8c3` GPL-3.0, `neovim@a1de074` Apache-2.0/Vim, `wezterm@f93d903` MIT, synthetic `vttest` corpora (< 8 KiB) — see `tmp/references/README.md` table.

Verification (read-only, no build):

```bash
git -C tmp/references/hyprland rev-parse HEAD  # c91fa5ab4d566206888c708dba66fca3646c382e
git -C tmp/references/waybar rev-parse HEAD    # 6d60c8e02be67bb85bb9b1ea803f2fbcf0722002
head -5 tmp/references/hyprland/LICENSE        # BSD 3-Clause
head -5 tmp/references/waybar/LICENSE          # MIT
```

`tmp/references/README.md` and `tmp/references/panel-tabs-research-2026-08-30.md` (73 lines) in the umbrella workspace hold the canonical revision table and distilled research; this file does not duplicate the clones.

## Exa research — distilled patterns (not TODO)

Three `exa_web_search_exa` queries on 2026-08-30, distilled as patterns Bitty could imitate later. No implementation in this task.

### Hyprland dwindle BSP (in-window tiling)

- Source: `src/layout/algorithm/tiled/dwindle/DwindleAlgorithm.cpp/.hpp` and `hyprgate` Rust port `dwindle.rs`. Binary split tree per workspace/monitor: each internal node `splitTop`, `splitRatio`, `box`, `children[2]`; leaves hold window target; `recalcSizePosRecursive` walks top-down; workspace root resets to `workArea()`.
- `insertion_plan` reconstructs topology from `(x,y,w,h)` by clean-split detection (vertical then horizontal) and emits `InsertStep`s for deterministic replay from rects alone. `resize_plan` restores internal ratios. Minimal imitable rule: `box.w * MULTIPLIER > box.h` → left/right else top/bottom, with explicit `Direction` override; `predictSizeForNewTarget` returns focused half-box. `layoutMsg` toggles `split`/`swap`/`rotate`/`movetoroot`/`splitratio`. Pure functions on `&[Win]` are directly imitable in `bitty-ui::layout` without C++ dependency.

### Waybar status provider (panel content)

- Source: `waybar/wiki/Writing-Modules`, `src/modules/custom.cpp`, `src/modules/cffi.cpp`. Module trait `AModule` → `ALabel`; registration in `meson.build` + `meson_options.txt` + `src/factory.cpp`. `dp.emit() → update()` → `ALabel::update()` via `set_markup`. Three provider shapes: polling (`interval` + `Sleeper_Thread`, bound interval ≥ 1s), `custom` exec (`exec`/`exec-if`/`interval`/`signal`/`return-type` json|raw, `{text,tooltip,class,percentage}`), and CFFI `dlopen` (`wbcffi_*`, `wbcffi_version=1`, `wbcffi_init_info {get_root_widget,queue_update}`) — avoid the last for Bitty tabs. Config is JSON (`format`, `tooltip-format`, `format-icons`, `max-length`, `on-click`, `menu-file`). Imitable as `PanelModule` trait `fn update(&mut self, ctx: &PanelCtx) -> PanelPatch` with explicit registration and `bitty-config` layered `PanelPlan`.

### winit vs smithay-client-toolkit layer-shell (out-of-window panel)

- Source: `winit` #2582/#2832/#4044/#2142 and `Smithay/smithay-client-toolkit` `src/shell/wlr_layer/mod.rs` plus `fono 4bc83bc` fallback. `winit` does not stably expose `wlr-layer-shell`; roles `xdg_toplevel` vs `zwlr_layer_surface_v1` are exclusive. Build the panel as a separate `sctk` `LayerSurface` (`LayerShell::bind` → `create_layer_surface(Layer, namespace, output)` → `LayerShellHandler {closed, configure}` with `ack_configure`), not a `winit::Window` attribute. Typical panel: `Layer::Top`, `Anchor::TOP|LEFT|RIGHT`, `exclusive_zone = panel_height`, `KeyboardInteractivity::OnDemand`. Compositor matrix: wlroots/KDE 5.27+/COSMIC/Wayfire/niri/labwc support `wlr-layer-shell`; GNOME Mutter does not — fall back to X11 override-redirect `_NET_WM_WINDOW_TYPE_NOTIFICATION` via `winit_x11` (`fono` `BackendId` `wlr → x11 → noop` table). Gate `wlr` behind `backend-wlr` cargo feature so `bitty-platform` `winit 0.30` stays `x11-only` on GNOME. Lifecycle: layer surfaces die on output disconnect; track `wl_output` separately; share `wgpu`/`softbuffer` via SCTK `SlotPool` (ARGB8888 `wl_shm`, double-buffered) without pulling SCTK into core `winit` build.

## What this means for Bitty workspace compositor

- Tabs/panes remain an in-window BSP inside `bitty-ui::layout::LayoutNode` / `bitty-runtime` (dwindle `splitTop`/`splitRatio`/`box` + `recalcSizePosRecursive`); persist layouts as rect lists and recompute via `insertion_plan` for replay/determinism.
- An out-of-window status panel is a separate SCTK `Layer::Top` bar (Waybar provider pattern) with `exclusive_zone` strut; main window stays `winit`. On GNOME, fall back to X11 override-redirect — do not block on `winit` layer-shell.
- No dependencies added; snapshots remain untrusted, excluded by umbrella `tmp/` routing and not referenced in `Cargo.toml`. Future work starts with pure `bitty-ui::layout` unit tests mirroring `hyprgate` cases, then a feature-gated `bitty-panel` crate behind `backend-wlr` following `fono` fallback.

## Evidence — file:line pointers (no defects)

- Umbrella research: `tmp/references/panel-tabs-research-2026-08-30.md:1`, `tmp/references/README.md:11`
- Clones: `tmp/references/hyprland` @ `c91fa5a`, `tmp/references/waybar` @ `6d60c8e`
- Issue: `gh issue view 120 --repo bitty-terminal/bitty` — `docs,area:ui,P0`, milestone `v0.1.0`
- This artifact: `docs/product/reference-notes.md:1` — research `status: draft`, not a TODO implementation claim

## Next (not in this task)

- No code in this task. Any future implementation starts with headless `bitty-ui::layout` unit tests for `insertion_plan`/`resize_plan` mirrors, then a `bitty-panel` crate behind `backend-wlr` with bounded `PanelModule` providers and per-output layer surfaces.
