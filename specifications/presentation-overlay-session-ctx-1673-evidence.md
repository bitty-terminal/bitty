# Presentation-only overlay session (CTX-1673; bitty#2849)

Implementation record for the Core presentation-only overlay session: a granted
plugin draws one bounded panel above the focused view with **no** input
capture. This file is repository-local implementation evidence and a short
manual for the surface; the normative contracts stay in `bitty-terminal-docs`
(`overlay-ownership-reconciliation.md`, `help-overlay-ownership-design.md`,
`chrome-surface-contract-candidate.md`,
`panel-content-scene-path-decision.md`). Nothing here promotes a status,
accepts an open question, or claims **Verified** behavior.

## What exists

| Layer      | Surface                                                     | Role                                                                                                                                                                                                                                                                                                                                      |
| ---------- | ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Capability | `ui.present`                                                | Draw a non-focusable presentation panel above the focused view. Its own head: separate from `ui.rich` (rich mount), `ui.overlay` (the `overlay` slot), and `ui.overlay.focus` (exclusive input capture). It is not preselected by any consent profile; a plugin requests it explicitly, so the v1 pinned profile contents stay unchanged. |
| Lua        | `bitty.ui.present.show(handle)` / `hide(handle)`            | Present or remove this generation's `overlay`-slot block. `bitty.ui.overlay.show`/`hide` remain as the CTX-1431 spelling of the same session.                                                                                                                                                                                             |
| Runtime    | `plugin_runtime::presentation_overlay::PresentationOverlay` | Single admitted `NonModal`/`Popup` surface per window, the followed view, and the bounded `overlay.dismissed` observation queue.                                                                                                                                                                                                          |
| Runtime    | `plugin_runtime::PluginRuntime::observe_presentation_focus` | Per-tick focus-follow: bind on first observation, re-bind on a focus switch, drop with `view-closed` when the bound view is gone.                                                                                                                                                                                                         |
| Runtime    | `runtime::present::paint_presentation_overlay`              | Paints the surface above view content and chrome bands, below the focusable capture surface, centered and clipped to the focused view.                                                                                                                                                                                                    |
| Runtime    | `Runtime::presentation_overlay_paint`                       | Per-frame accounting record: view, admitted rows, admitted bytes, truncation flag.                                                                                                                                                                                                                                                        |
| Runtime    | `Runtime::is_live_view`                                     | Liveness predicate for the focus-follow rule (layout leaf, pinned leaf, detached float).                                                                                                                                                                                                                                                  |

## Bounds

| Bound                              | Value                | Where                                                                |
| ---------------------------------- | -------------------- | -------------------------------------------------------------------- |
| Surfaces per window per generation | 1                    | `PresentationOverlay::show` (`E_UI_UNAVAILABLE` for a second plugin) |
| Admission kind / tier              | `NonModal` / `Popup` | `PRESENTATION_OVERLAY_KIND`, `PRESENTATION_OVERLAY_TIER`             |
| Rows per frame                     | 64                   | `PRESENTATION_OVERLAY_MAX_ROWS`                                      |
| Row text bytes per frame           | 16384                | `PRESENTATION_OVERLAY_MAX_BYTES`                                     |
| Pending dismissal observations     | 64, drop-oldest      | `PENDING_DISMISSED_MAX`                                              |
| Minimum frame width to paint       | 10 cells             | `PRESENTATION_OVERLAY_MIN_COLS`                                      |

The scene itself is additionally bounded by the accepted v1 scene budgets
enforced at mount, so the paint pass never flattens an unbounded tree.

## Behavior

- **No input capture.** The session never claims `OverlayCapture`, never
  requires `ui.overlay.focus`, never sets `modal_active`, and never receives key
  or pointer events. A click over the surface passes through to the grid
  beneath because the surface is not hit-tested.
- **No focus change.** Painting and dismissing are paint-only operations; the
  runtime focus ring is untouched.
- **Esc dismissal without consuming the press.** `Esc` dismisses the surface in
  `TerminalApp::intercept_chrome_key` before normal routing and then continues:
  the press still reaches a confirmation gate or the focused PTY. The owner
  observes exactly one `overlay.dismissed` event with
  `{ owner, handle, reason = "escape" }`; a live focusable capture session owns
  `Esc` until it ends.
- **Focus-follow.** The surface is bound to the focused view on the first tick
  after `show`, re-binds when focus moves, and is dropped with
  `{ reason = "view-closed" }` when the view it was drawn over is no longer
  live. A window with no focused view is not a close: the surface waits for the
  next focused view instead of dismissing itself.
- **No content reads.** The surface renders only the plugin's mounted scene. It
  reads no grid, scrollback, selection, clipboard, or terminal text, and it
  publishes no terminal content.
- **Teardown.** `hide` is idempotent and ungated so cleanup cannot wedge.
  Suspend, unload, disable, crash, rollback, and generation change drop the
  surface and release its admitted slot; a previous generation's handle is
  `E_UI_NOT_OWNER` and its next present paints nothing.
- **Safe mode.** Safe mode admits no plugin generation, so no surface exists; a
  generation that reaches the host under safe policy is refused with
  `E_UI_UNAVAILABLE` before admission.

## Errors

| Condition                                                  | Code                                       |
| ---------------------------------------------------------- | ------------------------------------------ |
| Missing `ui.present`                                       | `E_CAPABILITY_DENIED` (names `ui.present`) |
| Safe mode, no host surface, or a second plugin's admission | `E_UI_UNAVAILABLE`                         |
| Handle not this generation's `overlay`-slot block          | `E_UI_NOT_OWNER`                           |
| Non-integer handle                                         | `E_UI_COMPONENT_INVALID`                   |
| Call deadline exceeded                                     | `E_TIMEOUT`                                |

`bitty.ui.overlay.acquire` and its `E_UI_ALREADY_CAPTURED` single-owner
contract are unchanged; the split is in the _grant_, not in the capture
mechanism.

## Evidence

| Test                                                                                        | Proves                                                                                                                                          |
| ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `bitty-runtime` `tests/presentation_overlay_api.rs` `show_and_hide_present_without_capture` | present and hide with no capture grant, no capture session, no modal authority                                                                  |
| `...::one_presentation_surface_per_window`                                                  | one surface per window; a second plugin fails closed; owner re-show reuses its slot                                                             |
| `...::show_requires_present_grant_and_an_overlay_handle`                                    | `ui.rich` + `ui.overlay` never imply `ui.present`; non-`overlay` handles refuse; the denial names `ui.present`                                  |
| `...::escape_and_teardown_clear_the_surface`                                                | one `escape` observation; suspend/unload leave no surface                                                                                       |
| `...::dispose_and_generation_change_drop_the_surface`                                       | unload and reload fencing, admitted slot release                                                                                                |
| `...::focus_follow_binds_follows_and_drops_on_view_close`                                   | bind, follow, and `view-closed` drop                                                                                                            |
| `...::safe_mode_draws_no_presentation_surface`                                              | safe mode draws nothing                                                                                                                         |
| `bitty-runtime` `src/plugin_runtime/presentation_overlay.rs` unit tests                     | focus-follow state machine, one surface per view per generation, bounded observations                                                           |
| `bitty-runtime` `src/runtime/present.rs` `plugin_overlay_paint_tests`                       | centered paint above the focused frame, clipping, row/byte budget with reported tail drop, accounting cleared with the surface, focus unchanged |
| `bitty-lua` `tests/ui_bridge.rs` `present_namespace_show_hide_round_trips_through_host`     | the `bitty.ui.present` verbs route to the host, never claim capture, and reject a non-integer handle                                            |
| `bitty-terminal` `src/tests.rs` `presentation_overlay_esc_dismisses_and_reaches_pty`        | `Esc` does not consume the press; `0x1b` reaches the PTY; one queued observation                                                                |
| `...::presentation_overlay_esc_gate_precedence_keeps_the_consume`                           | a coincident paste/view-close gate keeps the consume                                                                                            |
| `...::presentation_overlay_binds_to_focus_and_drops_when_the_view_closes`                   | the tick binds to the focused view and a close drops the surface with `view-closed`                                                             |

## Open acceptance items

- **Independent security review is pending.** bitty#2849 requires a reviewer
  other than the author to record that the surface cannot read terminal
  content, cannot capture input, and cannot outlive its generation. This lane
  implements and tests those properties; it does not self-certify them.
- The final Lua spellings and the `ui.present` identifier remain candidates
  under `OQ-056`; this record states what the code does today, not what the
  corpus accepted.
- Animated entrance/exit is not implemented. If it lands, the accepted
  duration, easing, reduced-motion, and `--safe` collapse rules apply.
