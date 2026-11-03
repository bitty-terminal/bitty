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
| Runtime    | `Runtime::is_live_view`                                     | Liveness predicate for the focus-follow rule: true for a layout leaf, a pinned leaf, a detached float, a leaf stashed in an inactive workspace slot, or the leaf parked in the window scratchpad. |

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
| `...::show_accepts_exactly_one_grant_shape_and_hide_accepts_every_shape`                    | the capability-gate decision: one accepted grant shape draws, historical shapes deny, `hide` stays ungated                                      |
| `bitty-runtime` `src/runtime/present.rs` `...::bands_then_presentation_then_capture_stack_in_z_order` | z-order: chrome bands below the presentation panel below the focusable capture surface                                                  |
| `bitty-runtime` `src/runtime/workspaces.rs` `...::workspace_switch_keeps_stashed_views_live_issue_2849` | a leaf stashed into an inactive workspace still counts as existing                                                                     |
| `bitty-runtime` `src/plugin_runtime/presentation_overlay.rs` `...::workspace_switch_rebinds_instead_of_dismissing` | a workspace switch re-binds the panel and queues no `view-closed` observation                                                 |
| `bitty-runtime` `src/plugin_runtime/services.rs` `...::presentation_safe_mode_refuses_before_the_grant_check_and_admission` | safe mode refuses before the grant check and before any admission                                    |

## Capability gate decision (criterion 4)

`bitty.ui.overlay.show` required the historical `ui.rich` + `ui.overlay` pair
before this work (`bitty` `f0a6d252`) and requires the dedicated `ui.present`
head after it (`bitty` `8108e4f7`). **The re-gate is deliberate; no
compatibility alias is restored.**

- PR #2856 states the shared gate as the design in both its body ("`bitty.ui.overlay.show`/`hide` remain as the CTX-1431
  spelling of the same session and are gated by the same head") and its commit
  message, so the changed grant is the stated intent rather than an oversight.
- The published plugin API reference records the same rule for `8108e4f7`
  (`bitty-plugins-docs` `sdk/reference/ui.md`, "Presentation-only overlay
  (`ui.present`)"): `ui.present` alone authorizes drawing, and both spellings of
  the session are gated by it.
- The change is unreleased: `v0.0.31` does not contain `f0a6d252`, so no shipped
  plugin was ever granted drawing through the pair.
- An OR-gate accepting `ui.rich` + `ui.overlay` would let a manifest draw by
  asking to mount, which is exactly the coupling the separate `ui.present`
  spelling exists to break.

`show_accepts_exactly_one_grant_shape_and_hide_accepts_every_shape` locks it: the
minimal accepted shape is `ui.rich` + `ui.overlay` + `ui.present` (adding
`ui.overlay.focus` changes nothing), the historical pair is denied with
`E_CAPABILITY_DENIED` naming `ui.present`, and `hide` is ungated under every
shape.

## Independent security review (criterion 3)

**Completed.** A reviewer that did not author the implementation reviewed
`origin/main` at `8108e4f7` (the squash of PR #2856) read-only — no code or
branch was modified — and recorded the three properties this criterion asks
for with the file-and-line evidence behind each:

- **Cannot read terminal content.** The paint path renders only the plugin's own
  mounted `UiNode` (`runtime/present.rs`), and the dismiss payload carries no
  bytes, text, or coordinates. The one Core-side read of
  `layers.combined_glyphs` is geometry used to occlude underlay glyphs inside
  the card; it is never exposed to the plugin.
- **Cannot capture input.** No capture session is claimed, no key or pointer
  event is delivered, no hit-testing exists, and the focus ring never moves.
  The `Esc` that dismisses the panel still reaches the focused PTY; a plugin
  that also wants raw keys needs a separate `terminal.input.*` grant.
- **Cannot outlive its generation.** Suspend, dispose/unload/crash, and failed
  activation all funnel through `revoke_plugin` → `clear()`, which drops the
  session and releases the admitted slot; paint content is re-resolved every
  tick from the live generation's retained blocks, so a revoked generation has
  nothing to paint.

Records:

- Independent review (issue comment, 2026-11-03):
  <https://github.com/bitty-terminal/bitty/issues/2849#issuecomment-6603095333>
- The same review as the PR review:
  <https://github.com/bitty-terminal/bitty/pull/2856#pullrequestreview-5751722036>
- Revision-locked record in the canonical docs corpus: `bitty-terminal-docs`
  `security/audits/presentation-overlay-2026-11.md`.

Residual gaps that review named — closed here, or still standing:

| Residual gap                                                           | Status                                                                                                                          |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| Required security-review artifact missing                              | Closed: this section plus `bitty-terminal-docs` `security/audits/presentation-overlay-2026-11.md`.                                |
| `bitty.ui.overlay.show` re-gated from `ui.rich` + `ui.overlay`          | Decided and locked with a test; see the capability gate decision above.                                                          |
| Stale in-code docs still claim the mount grants                        | Closed: `bitty-lua/src/host.rs` and `plugin_runtime/presentation_overlay.rs` now name `ui.present`.                              |
| A workspace switch reported as `view-closed`                           | Closed: `Runtime::is_live_view` answers existence across inactive workspace slots and the scratchpad, so a switch re-binds.      |
| Z-order asserted only by call order                                    | Closed: `plugin_overlay_paint_tests::bands_then_presentation_then_capture_stack_in_z_order`.                                    |
| Safe-mode branch had no direct test                                    | Closed: `services::tests::presentation_safe_mode_refuses_before_the_grant_check_and_admission`.                                  |
| One-frame ghost after a cold-path revoke                               | Open. Session and slot drop immediately, but the cloned `Runtime` paint copy clears on the next tick. Bounded at one frame.      |
| Budget accounting is local to the presentation session                 | Open. The session's own `OverlayManager` bounds its one slot and does not bound overlays held by the window's live manager.      |
| `zh-CN` manual page lacks the `ui.present` section                     | Open in `bitty-manual`; the English page carries the section.                                                                    |

## Open acceptance items

- **Independent security review: closed** — see the section above; the review
  found no terminal-content read, no input capture, and no outliving of the
  generation, and its residual gaps are tracked in the table there.
- The final Lua spellings and the `ui.present` identifier remain candidates
  under `OQ-056`; this record states what the code does today, not what the
  corpus accepted.
- Animated entrance/exit is not implemented. If it lands, the accepted
  duration, easing, reduced-motion, and `--safe` collapse rules apply.
