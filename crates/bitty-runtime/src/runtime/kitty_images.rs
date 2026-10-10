//! `Runtime` — Kitty graphics display routing (issue #1802).
//!
//! Split from `super` (`runtime.rs`) as a pure addition: the VT parser
//! still pre-scans `APC G`, base64-unwraps, and reassembles `m=` chunks
//! under the ledger cap, and this module turns a completed transmission's
//! parameters plus payload bytes into a stored image and, for display
//! actions, a cursor-anchored placement.
//!
//! Core owns the bounded decode ([`bitty_rich::kitty_decode`]: PNG via the
//! `image` codec edge, raw RGB/RGBA inline; 8192 px/side, 4096 x 4096 px
//! area, 64 MiB RGBA) and the raster step ([`bitty_rich::kitty_raster`]:
//! uncached nearest-neighbor; the `bitty-graphics` extension holds its own
//! copies plus a raster cache, and Core never depends on extension
//! internals). Placement policy (admission, per-origin quotas, origin
//! tagging, alternate-screen suppression) is unchanged and unit-tested in
//! `bitty-rich`.
//!
//! Stored bytes count against the transmitting stream's origin (S5
//! per-origin quotas, #1849): the transmit seams pass `self.kitty_origin`
//! into the layer, so a noisy pane evicts only its own oldest images and
//! global pressure refuses without evicting a victim.
//!
//! Display anchors at the drained stream's cursor cell (the primary grid,
//! or the pane session swapped in by `handle_pane_bytes`) with that
//! grid's `State::scrollback_len()` as the scroll base (images scroll with
//! content; see [`bitty_rich::kitty_place`]). The placement keeps the
//! stream's origin token so the present layer confines it to its own leaf
//! (CTX-0254). While the alternate screen is active, transmissions decode
//! and store but never place (fail closed, same shape as transmit-only).
//!
//! Cell reservation (issue #1802, second bug): a successful display
//! advances the cursor past the placement span (unless `C=1`), so
//! subsequently printed text starts after the image instead of over it;
//! at present time images composite topmost over grid cells. Grid truth
//! itself is never mutated for images (placement records live in the
//! image layer, not in cells), except for app-emitted `U+10EEEE`
//! placeholder runs, which occupy ordinary cells by design.
//!
//! Queries (`a=q`) are answered here (S2, #1849) in spec-exact wire
//! shape (`recording/references/kitty@0b12ed8`, GPL-3.0-only:
//! `docs/graphics-protocol.rst:430,476,480,500`,
//! `kitty/graphics.c:927-951,2540-2549`,
//! `kitty_tests/graphics.py:40-52`): support probes (`i=` plus payload
//! bytes) test-load through the declared-size pre-check without storing,
//! answering `Gi=<id>;OK` or `Gi=<id>;EINVAL:<msg>`/`;ENOSPC:<msg>`;
//! payload-less `i=`/`I=`/`p=` lookups (bitty extension, no Kitty
//! counterpart, kept only in spec shape) answer `Gi=...;OK` or
//! `;ENOENT:<msg>`. At most one bounded reply per query, honoring `q=`
//! suppression (see [`Runtime::answer_kitty_query`]); queries without
//! `i=`/`I=` stay silent like kitty's `REPORT_ERROR`-with-no-reply.
//!
//! Still deferred (blocked, recorded in the PR body): `a=p` virtual
//! (`U=1`) store bookkeeping beyond the grid-cell runs, animation
//! (`a=f`/`a=a`/`a=c`), local mediums (`t=f`/`t=t`/`t=s`
//! file reads), the raster cache, and
//! cursor-on-top compositing.//!
//! Display anchors at the drained stream's cursor cell (the primary grid,
//! or the pane session swapped in by `handle_pane_bytes`) with that
//! grid's `State::scrollback_len()` as the scroll base (images scroll with
//! content; see [`bitty_rich::kitty_place`]). The placement keeps the
//! stream's origin token so the present layer confines it to its own leaf
//! (CTX-0254). While the alternate screen is active, transmissions decode
//! and store but never place (fail closed, same shape as transmit-only).
//!
//! Unicode placeholders (CTX-0821, issue #1400) resolve at print time in
//! [`bitty_term_state::State`]: `U+10EEEE` cells carry the pen colors and
//! combining diacritics, decoded headlessly via
//! [`bitty_term_state::kitty_unicode`] (`State::kitty_unicode_run_at`,
//! `State::kitty_unicode_runs_on_row`) and sized via
//! [`bitty_rich::kitty_unicode`] (`unicode_run_rect`). This module adds
//! the runtime delete seam: [`Runtime::kitty_unicode_delete`] clears the
//! named runs' grid cells ([`bitty_term_state::State::kitty_unicode_clear`]),
//! so `a=d,d=i[,p=]` has deterministic grid-text semantics. Stored-image
//! and placement-layer bookkeeping for the `U=1` virtual prototype itself
//! stays follow-up work (recorded in the PR body).

use super::*;

/// Typed Kitty display-routing rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyImageError {
    /// Wire `f=` value maps to no supported format (never guessed).
    UnknownFormat(u32),
    /// The Core pre-check refused the declared payload, or the Core-owned
    /// decoder refused the bytes. No bitmap, no placement.
    Decode(bitty_rich::KittyPrecheckError),
    /// Store/placement admission refused an otherwise decoded image.
    Placement(bitty_rich::KittyPlacementError),
}

impl std::fmt::Display for KittyImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFormat(value) => write!(f, "kitty image unknown format f={value}"),
            Self::Decode(err) => write!(f, "kitty image decode: {err}"),
            Self::Placement(err) => write!(f, "kitty image placement: {err}"),
        }
    }
}

impl std::error::Error for KittyImageError {}

/// Outcome of [`Runtime::kitty_display_image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyDisplayOutcome {
    /// Transmit-only (`a=t`): decoded and stored, never placed.
    Stored {
        /// Handle of the stored image.
        image: bitty_rich::KittyImageId,
    },
    /// Transmit-and-display (`a` absent or `a=T`): stored and placed at
    /// the cursor cell.
    Displayed {
        /// Handle of the stored image.
        image: bitty_rich::KittyImageId,
        /// Handle of the new placement.
        placement: bitty_rich::KittyPlacementId,
    },
    /// Unsupported `a=` value: stored, **not painted** (fail closed).
    StoredNotDisplayed {
        /// Handle of the stored image.
        image: bitty_rich::KittyImageId,
    },
    /// Alternate screen active: decoded and stored, never placed.
    SuppressedAlternateScreen {
        /// Handle of the stored image.
        image: bitty_rich::KittyImageId,
    },
}

impl Runtime {
    /// Number of stored decoded Kitty images (headless-observable).
    #[must_use]
    pub fn kitty_image_count(&self) -> usize {
        self.kitty_images.len()
    }

    /// Number of retained Kitty placements (headless-observable).
    #[must_use]
    pub fn kitty_placement_count(&self) -> usize {
        self.kitty_images.placement_len()
    }

    /// Image blits composited on the last presented frame (CTX-0252 F2).
    ///
    /// Latched on every successful present; idle ticks leave it unchanged.
    /// Bound by [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`]. Image
    /// and placement counts above stay headless-observable alongside it.
    #[must_use]
    pub fn kitty_last_frame_images(&self) -> usize {
        self.kitty_last_frame_images
    }

    /// Total encoded bytes buffered across all in-flight Kitty graphics streams
    /// (primary parser + all pane parsers). Used to enforce IMG-4 (256 MiB
    /// in-flight cap) before buffering more input. Returns 0 when all parsers
    /// are idle (CTX-0904).
    #[must_use]
    pub(super) fn total_kitty_inflight_bytes(&self) -> usize {
        let primary = self.parser.pending_kitty_encoded_bytes();
        let panes: usize = self
            .pane_sessions
            .values()
            .map(|sess| sess.parser.pending_kitty_encoded_bytes())
            .sum();
        primary + panes
    }

    /// Decodes and stores a Kitty payload without placing it.
    ///
    /// `format_f` is the wire `f=` value (`100` PNG, `24` RGB, `32` RGBA);
    /// `width_s`/`height_v` are the wire `s`/`v` dimensions (required for
    /// raw formats, ignored for PNG). The Core-retained declared-size
    /// pre-check ([`bitty_rich::precheck_declared_image`]) runs before any
    /// allocation; the Core-owned decoder ([`bitty_rich::kitty_decode`])
    /// then produces the RGBA8 bitmap the image layer stores under the
    /// per-origin store quotas (S5, #1849: FIFO eviction within the
    /// transmitting origin only, global pressure refuses without evicting
    /// a victim). The bytes count against the currently drained stream's
    /// origin (`self.kitty_origin`: `None` primary, `Some` pane session).
    ///
    /// # Errors
    ///
    /// [`KittyImageError::UnknownFormat`] for unsupported `f=` values,
    /// [`KittyImageError::Decode`] for empty, underspecified, oversize,
    /// length-mismatched, or malformed payloads (before allocation where
    /// the bound allows), [`KittyImageError::Placement`] when the decoded
    /// bitmap exceeds the layer caps or the global bound is held by other
    /// origins. Failures store nothing and evict nothing.
    pub fn kitty_transmit_image(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload: &[u8],
        _compressed_len: usize,
    ) -> Result<bitty_rich::KittyImageId, KittyImageError> {
        Self::precheck_transmit(format_f, width_s, height_v, payload.len())?;
        let decoded = bitty_rich::decode_kitty_payload(format_f, width_s, height_v, payload)
            .map_err(|err| KittyImageError::Decode(bitty_rich::KittyPrecheckError::from(err)))?;
        let compressed_len = payload.len();
        let origin = self.kitty_origin;
        self.kitty_images
            .store_for_origin(
                decoded.width,
                decoded.height,
                decoded.rgba,
                compressed_len,
                origin,
            )
            .map_err(KittyImageError::Placement)
    }

    /// Decodes and stores a Kitty image with owned payload.
    ///
    /// Same behavior as [`Self::kitty_transmit_image`]; the owned buffer
    /// moves into the stored bitmap without copying for `f=32` (and
    /// expands in place for `f=24`). The bytes count against the currently
    /// drained stream's origin, like [`Self::kitty_transmit_image`].
    pub fn kitty_transmit_image_owned(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload: Box<[u8]>,
        _compressed_len: usize,
    ) -> Result<bitty_rich::KittyImageId, KittyImageError> {
        let wire_len = payload.len();
        Self::precheck_transmit(format_f, width_s, height_v, wire_len)?;
        let decoded = bitty_rich::kitty_decode::decode_kitty_payload_owned(
            format_f, width_s, height_v, payload,
        )
        .map_err(|err| KittyImageError::Decode(bitty_rich::KittyPrecheckError::from(err)))?;
        let origin = self.kitty_origin;
        self.kitty_images
            .store_for_origin(
                decoded.width,
                decoded.height,
                decoded.rgba,
                wire_len,
                origin,
            )
            .map_err(KittyImageError::Placement)
    }

    /// Shared wire-format admission + declared-size pre-check.
    ///
    /// Maps `f=` without guessing, then runs the Core-retained
    /// pre-allocation validator. No pixel buffer exists at any point.
    fn precheck_transmit(
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload_len: usize,
    ) -> Result<(), KittyImageError> {
        let channels = match format_f {
            bitty_rich::KITTY_FORMAT_PNG => None,
            bitty_rich::KITTY_FORMAT_RGB => Some(3),
            bitty_rich::KITTY_FORMAT_RGBA => Some(4),
            _ => return Err(KittyImageError::UnknownFormat(format_f)),
        };
        bitty_rich::precheck_declared_image(channels, width_s, height_v, payload_len)
            .map_err(KittyImageError::Decode)
    }

    /// Decodes, stores and — for display actions outside the alternate
    /// screen — places a Kitty image at the drained cursor cell.
    ///
    /// `action_a` is the wire `a=` value (`None` when absent, which means
    /// transmit-and-display per the kitty specification); `cols_c`/`rows_r`
    /// are the explicit `c=`/`r=` cell spans (0 derives from pixels).
    /// `cursor_movement_c` is the wire `C=` value (`0` moves cursor, `1` keeps it).
    /// `z` orders images ascending among themselves.
    ///
    /// The placement is tagged with the currently-drained stream's origin
    /// (`self.kitty_origin`: `None` primary, `Some` pane session swapped
    /// in by `handle_pane_bytes`), and the cursor/scrollback base come
    /// from that same swapped-in grid — so a pane's image anchors to the
    /// pane's cursor and paints only on the pane's leaf (CTX-0254
    /// cross-pane spoof prevention). Alternate-screen suppression likewise
    /// reads the drained grid.
    ///
    /// A successful display forces a full redraw so the next tick paints
    /// the new placement. Alternate-screen display stores without placing
    /// ([`KittyDisplayOutcome::SuppressedAlternateScreen`]).
    ///
    /// Per Kitty spec: after placing an image, cursor moves right by the
    /// number of columns and down by the number of rows in the placement
    /// rectangle, unless `C=1` is set.
    ///
    /// # Errors
    ///
    /// Same as [`Runtime::kitty_transmit_image`]; failures store nothing
    /// and place nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_image(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        payload: &[u8],
        z: i32,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        self.kitty_display_image_with_wire(
            format_f,
            width_s,
            height_v,
            action_a,
            cols_c,
            rows_r,
            cursor_movement_c,
            payload,
            z,
            0,
            0,
        )
    }

    /// Decodes, stores and places with origin-scoped wire identity.
    ///
    /// Same behavior as [`Self::kitty_display_image`] except the rendered
    /// placement records the wire `i=`/`p=` ids (`0` when absent), so a
    /// later protocol-level deletion (`a=d,d=i`) clears it via
    /// [`Self::kitty_delete_rendered_image`] instead of leaving an orphan
    /// blit (CTX-1072, #1850).
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_image_with_wire(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        payload: &[u8],
        z: i32,
        wire_image: u32,
        wire_placement: u32,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        let action = bitty_rich::KittyAction::from_a(action_a);
        let alt_active = self.state.alt_screen_active();
        if action.displays() && !alt_active {
            // Atomic transmit-and-display (S5, #1849): preflight placement
            // admission before decoding/storing, so a placement-quota
            // refusal stores nothing and never discards FIFO-evicted
            // images (a post-store rollback could not restore evictions).
            // Transmit-only, unsupported actions, and alternate-screen
            // display keep storing without placing.
            self.kitty_images
                .check_placement_quota_for_origin(self.kitty_origin)
                .map_err(KittyImageError::Placement)?;
        }
        let compressed_len = payload.len();
        let image =
            self.kitty_transmit_image(format_f, width_s, height_v, payload, compressed_len)?;
        if alt_active {
            return Ok(KittyDisplayOutcome::SuppressedAlternateScreen { image });
        }
        if !action.displays() {
            return Ok(if matches!(action, bitty_rich::KittyAction::Transmit) {
                KittyDisplayOutcome::Stored { image }
            } else {
                KittyDisplayOutcome::StoredNotDisplayed { image }
            });
        }
        let cursor = self.state.cursor().position;
        let metrics = self.live_cell_metrics();
        let rich_metrics = bitty_rich::CellMetrics {
            width: metrics.width,
            height: metrics.height,
        };
        let placement_id = self
            .kitty_images
            .display_for_origin_with_wire(
                image,
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                rich_metrics,
                self.state.scrollback_len(),
                z,
                self.kitty_origin,
                wire_image,
                wire_placement,
            )
            .map_err(KittyImageError::Placement)?;
        self.pending_full_redraw = true;

        // Per Kitty spec: after placing an image, the cursor moves right by the
        // number of columns and down by the number of rows in the placement
        // rectangle, unless C=1 is explicitly set. Use the effective placement
        // dimensions (what was actually rendered), not the requested spans which
        // may be zero when omitted.
        if cursor_movement_c != 1 {
            self.advance_cursor_past_placement(
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                placement_id,
            );
        }

        Ok(KittyDisplayOutcome::Displayed {
            image,
            placement: placement_id,
        })
    }

    /// Moves the cursor past the placement span after a display.
    ///
    /// Cell math is 0-based; the [`bitty_vt::TerminalAction::CursorPosition`]
    /// action is 1-based `CUP`, so the targets shift up by one here
    /// (saturating; the state machine clamps to the grid anyway). Without
    /// the shift the cursor lands one cell too early and the next printed
    /// text overlaps the image's last column/row (issue #1802).
    ///
    /// Security bound (CTX-1072, #1850): the wire `r=` span is untrusted
    /// PTY input, so scroll driven past the bottom is capped to the
    /// trusted viewport height and to
    /// [`bitty_rich::KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT`],
    /// whichever is smaller. A maximal-row (`r=65535`) placement of a 1x1
    /// image therefore scrolls at most one screen instead of driving
    /// about 65k linefeeds per placement. In-budget placements (overflow
    /// within one screen) behave exactly as before.
    fn advance_cursor_past_placement(
        &mut self,
        cursor_col: u16,
        cursor_row: u16,
        cols_c: u16,
        rows_r: u16,
        placement_id: bitty_rich::KittyPlacementId,
    ) {
        // Retrieve the actual placement to get effective dimensions.
        let placement = self
            .kitty_images
            .get_placement(placement_id)
            .expect("placement just created must exist");

        let effective_cols = if cols_c > 0 { cols_c } else { placement.cols };
        let effective_rows = if rows_r > 0 { rows_r } else { placement.rows };
        let new_col = cursor_col.saturating_add(effective_cols);
        let target_row = cursor_row.saturating_add(effective_rows);
        let viewport_rows = u16::try_from(self.state.height()).unwrap_or(u16::MAX);
        let max_row = viewport_rows.saturating_sub(1);
        if target_row > max_row {
            let overflow = target_row - max_row;
            let viewport_cap = viewport_rows.max(1);
            let lines = overflow
                .min(viewport_cap)
                .min(bitty_rich::KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT);
            for _ in 0..lines {
                self.state.apply(&bitty_vt::TerminalAction::PrintControl(
                    bitty_vt::ControlChar(0x0A),
                ));
            }
            self.state.apply(&bitty_vt::TerminalAction::CursorPosition {
                row: bitty_vt::Row(max_row.saturating_add(1)),
                col: bitty_vt::Col(new_col.saturating_add(1)),
            });
        } else {
            self.state.apply(&bitty_vt::TerminalAction::CursorPosition {
                row: bitty_vt::Row(target_row.saturating_add(1)),
                col: bitty_vt::Col(new_col.saturating_add(1)),
            });
        }
    }

    /// Decodes, stores and places a Kitty image with owned payload.
    ///
    /// Same behavior as [`Self::kitty_display_image`] but moves the
    /// payload to avoid an intermediate copy on raw streams.
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_image_owned(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        payload: Box<[u8]>,
        z: i32,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        self.kitty_display_image_owned_with_wire(
            format_f,
            width_s,
            height_v,
            action_a,
            cols_c,
            rows_r,
            cursor_movement_c,
            payload,
            z,
            0,
            0,
        )
    }

    /// Owned variant of [`Self::kitty_display_image_with_wire`].
    ///
    /// Same behavior but moves the payload to avoid an intermediate copy
    /// on raw streams, while recording the wire `i=`/`p=` identity for
    /// later protocol-level deletion (CTX-1072, #1850).
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_image_owned_with_wire(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        payload: Box<[u8]>,
        z: i32,
        wire_image: u32,
        wire_placement: u32,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        let action = bitty_rich::KittyAction::from_a(action_a);
        let alt_active = self.state.alt_screen_active();
        if action.displays() && !alt_active {
            // Atomic transmit-and-display (S5, #1849): preflight placement
            // admission before decoding/storing, so a placement-quota
            // refusal stores nothing and never discards FIFO-evicted
            // images (a post-store rollback could not restore evictions).
            // Transmit-only, unsupported actions, and alternate-screen
            // display keep storing without placing.
            self.kitty_images
                .check_placement_quota_for_origin(self.kitty_origin)
                .map_err(KittyImageError::Placement)?;
        }
        let compressed_len = payload.len();
        let image =
            self.kitty_transmit_image_owned(format_f, width_s, height_v, payload, compressed_len)?;
        if alt_active {
            return Ok(KittyDisplayOutcome::SuppressedAlternateScreen { image });
        }
        if !action.displays() {
            return Ok(if matches!(action, bitty_rich::KittyAction::Transmit) {
                KittyDisplayOutcome::Stored { image }
            } else {
                KittyDisplayOutcome::StoredNotDisplayed { image }
            });
        }
        let cursor = self.state.cursor().position;
        let metrics = self.live_cell_metrics();
        let rich_metrics = bitty_rich::CellMetrics {
            width: metrics.width,
            height: metrics.height,
        };
        let placement_id = self
            .kitty_images
            .display_for_origin_with_wire(
                image,
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                rich_metrics,
                self.state.scrollback_len(),
                z,
                self.kitty_origin,
                wire_image,
                wire_placement,
            )
            .map_err(KittyImageError::Placement)?;
        self.pending_full_redraw = true;

        if cursor_movement_c != 1 {
            self.advance_cursor_past_placement(
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                placement_id,
            );
        }

        Ok(KittyDisplayOutcome::Displayed {
            image,
            placement: placement_id,
        })
    }

    /// Deletes rendered placements by origin-scoped wire identity.
    ///
    /// Origin-scoped counterpart to the terminal-truth `a=d,d=i` delete:
    /// removes rendered placements of the currently drained stream
    /// (`self.kitty_origin`) naming `(image_id, placement_id)`, so a
    /// protocol-level deletion reliably clears displayed output instead
    /// of leaving an orphan blit (CTX-1072, #1850). `None` clears every
    /// placement of the image; `Some(p)` clears only the pinned one
    /// (anonymous placements never match a pin). Stored images stay
    /// inert under the store caps. Returns the removed count. A positive
    /// count forces a full redraw so the next tick drops the blit.
    pub fn kitty_delete_rendered_image(
        &mut self,
        image_id: u32,
        placement_id: Option<u32>,
    ) -> usize {
        let removed = self
            .kitty_images
            .delete_by_wire(self.kitty_origin, image_id, placement_id);
        if removed > 0 {
            self.pending_full_redraw = true;
        }
        removed
    }

    /// Answers one Kitty `a=q` query with at most one bounded reply (S2, #1849).
    ///
    /// Spec-exact wire shape (`recording/references/kitty@0b12ed8`,
    /// GPL-3.0-only; see [`crate::queries`]): `;` separator, `OK` or
    /// `CODE:msg`, identity echo (`i=`/`I=`/`p=`), never `s=`/`v=`/`f=`.
    /// The parser already reassembles the (single-shot) command and
    /// validates the query shape (`f=` mandatory, `q=` 0/1/2); this seam
    /// only answers. Queries never store, place, evict, or allocate pixel
    /// buffers:
    ///
    /// - Support probe, spec (`i=` non-zero plus payload bytes): the
    ///   terminal tries to load the image and answers whether loading
    ///   was successful, without storing or replacing
    ///   (`docs/graphics-protocol.rst:440-442`). Test-loads the
    ///   declaration through [`bitty_rich::precheck_declared_image`]:
    ///   unknown `f=` and refused declarations answer
    ///   `Gi=<id>;EINVAL:<msg>`, quota-full stores answer
    ///   `Gi=<id>;ENOSPC:store full`, otherwise `Gi=<id>;OK`. A `p=` on a
    ///   probe is ignored (kitty `graphics.c:2549` echoes only `q_iid`).
    ///   Probes REQUIRE `i=` (spec-exact, decided per the open point):
    ///   kitty `REPORT_ERROR`s with no reply when `!q_iid`
    ///   (`graphics.c:2540-2543`), and bitty stays silent too — no
    ///   `f=`-echo extension is kept, because it would break the client's
    ///   `partition(';')` parser with zero interop value while native
    ///   clients always send `i=` per the spec example.
    /// - Payload-less status lookup, bitty extension in spec shape (no
    ///   Kitty counterpart: kitty `a=q` always test-loads bytes):
    ///   `i=` non-zero with empty payload, or `I=` (resolved through
    ///   terminal truth like the `d=n` delete path; payload ignored),
    ///   looks up the origin-scoped wire identity
    ///   (`self.kitty_origin`: `None` primary, `Some` pane session)
    ///   against the S5 quota-scoped store, mirroring protocol deletion
    ///   matching — image-level without `p=`, one pinned placement with
    ///   `p=` (`p=0` behaves as absent, as at the delete call site;
    ///   `i=0`/unresolvable numbers name nothing held). answers
    ///   `Gi=...;OK` or `Gi=...;ENOENT:not held`. Kitty-native clients
    ///   always send payload plus `i=`, so they never hit this path and
    ///   are unaffected.
    /// - No `i=`/`I=`: silence (no reply built), matching kitty's
    ///   `REPORT_ERROR`-with-no-reply. Suppression is irrelevant there.
    ///
    /// The reply (when built) is one fixed-format `APC G` answer (always
    /// < 1 KiB) queued through the bounded `Reply` action, so the 4 KiB
    /// reply cap (drop-whole past the cap plus overflow flag) and the
    /// `poll_pty -> write_replies` flush path apply unchanged. Wire `q=`
    /// suppression is honored: `0` replies, `1` suppresses `OK`, `2`
    /// suppresses failures too (the parser admits only 0/1/2 on `a=q`;
    /// anything else suppresses defensively like `2`). Failures are
    /// silent protocol answers, never stderr: a probing client must not
    /// flood diagnostics.
    ///
    /// Time O(placements) on a scan bounded by the layer caps; space O(1)
    /// besides the sub-1-KiB reply.
    pub(crate) fn answer_kitty_query(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload: &[u8],
        control: bitty_vt::KittyControlKeys,
    ) {
        let Some((reply, ok)) =
            self.kitty_query_verdict(format_f, width_s, height_v, payload, &control)
        else {
            // Spec-exact silence for queries without `i=`/`I=` (kitty
            // `REPORT_ERROR` with no reply).
            return;
        };
        let suppressed = control.quiet >= 2 || (control.quiet == 1 && ok);
        if suppressed {
            return;
        }
        self.state.apply(&TerminalAction::Reply {
            bytes: reply.into_boxed_slice(),
        });
    }

    /// Computes one query verdict: the reply bytes plus whether it is `OK`.
    ///
    /// Returns `None` for spec-exact silence (no `i=`/`I=` identity).
    /// Pure besides the terminal-truth number lookup; the caller owns
    /// suppression and queueing (see [`Self::answer_kitty_query`]).
    fn kitty_query_verdict(
        &self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload: &[u8],
        control: &bitty_vt::KittyControlKeys,
    ) -> Option<(Vec<u8>, bool)> {
        // `I=` queries (mutually exclusive with `i=` per the parser) are
        // always payload-less status lookups (extension): the number
        // resolves through terminal truth, the payload (if any) is
        // ignored, and the reply echoes the queried `I=` plus the
        // resolved `i=` (transmit echo pattern). Kitty-native clients
        // never send `I=` on queries, so they are unaffected.
        if control.image_id == 0 && control.image_number != 0 {
            let queried = control.image_number;
            // `I=` numbers resolve through terminal truth like the
            // `d=n` delete path; unresolvable numbers resolve to `0`,
            // which is never held (anonymous images are not
            // addressable, as for deletion).
            let resolved = self
                .state
                .kitty_placements()
                .newest_with_number(queried)
                .unwrap_or(0);
            // `p=0` behaves as absent (image-level), as at the delete
            // call site: only a non-zero pin addresses one placement.
            let pin = if control.placement_id == 0 {
                None
            } else {
                Some(control.placement_id)
            };
            if resolved != 0 && self.kitty_status_held(resolved, pin) {
                return Some((
                    crate::queries::kitty_status_ok_reply(resolved, queried, pin),
                    true,
                ));
            }
            return Some((
                crate::queries::kitty_status_enoent_reply(resolved, queried, pin),
                false,
            ));
        }
        if control.image_id != 0 {
            if payload.is_empty() {
                // Payload-less `i=` status lookup (extension in spec
                // shape; kitty-native clients always send payload).
                let pin = if control.placement_id == 0 {
                    None
                } else {
                    Some(control.placement_id)
                };
                if self.kitty_status_held(control.image_id, pin) {
                    return Some((
                        crate::queries::kitty_status_ok_reply(control.image_id, 0, pin),
                        true,
                    ));
                }
                return Some((
                    crate::queries::kitty_status_enoent_reply(control.image_id, 0, pin),
                    false,
                ));
            }
            // Support probe, spec (`i=` plus payload): test-load without
            // storing; `p=` (if any) is ignored per kitty `:2549`.
            let image_id = control.image_id;
            let channels = match format_f {
                bitty_rich::KITTY_FORMAT_PNG => None,
                bitty_rich::KITTY_FORMAT_RGB => Some(3),
                bitty_rich::KITTY_FORMAT_RGBA => Some(4),
                _ => {
                    return Some((
                        crate::queries::kitty_probe_einval_reply(image_id, "unknown format"),
                        false,
                    ));
                }
            };
            if bitty_rich::precheck_declared_image(channels, width_s, height_v, payload.len())
                .is_err()
            {
                return Some((
                    crate::queries::kitty_probe_einval_reply(image_id, "bad data"),
                    false,
                ));
            }
            // The decoded RGBA size is exact for raw claims (the pre-check
            // enforces exact length, so `s`/`v` are present here); PNG
            // decodes to an `IHDR`-governed size the probe cannot know
            // without allocating, so only the count quotas apply to it.
            let decoded_bytes = channels.map(|_| {
                let pixels = u64::from(width_s.unwrap_or(0))
                    .saturating_mul(u64::from(height_v.unwrap_or(0)));
                usize::try_from(pixels.saturating_mul(4))
                    .unwrap_or(bitty_rich::KITTY_DECODE_MAX_BYTES)
            });
            if !self.kitty_probe_admittable(decoded_bytes) {
                return Some((
                    crate::queries::kitty_probe_enospc_reply(image_id, "store full"),
                    false,
                ));
            }
            return Some((crate::queries::kitty_probe_ok_reply(image_id), true));
        }
        // No `i=`/`I=`: spec-exact silence (kitty `REPORT_ERROR` with no
        // reply, `graphics.c:2540-2543`). No `f=`-echo extension is kept:
        // see [`crate::queries::kitty_probe_ok_reply`] for the rationale.
        None
    }

    /// Whether one origin-scoped wire identity is held (S2, #1849).
    ///
    /// Mirrors protocol-deletion matching against the S5 quota-scoped
    /// store: only the draining stream's origin (`self.kitty_origin`)
    /// answers `held`, so one pane's images never leak into another pane's
    /// status. Placements whose image was evicted fail closed (`false`,
    /// like dangling lookups at paint time). Bounded scan over the
    /// capped placement deque; no allocation.
    fn kitty_status_held(&self, image_id: u32, placement: Option<u32>) -> bool {
        if image_id == 0 {
            return false;
        }
        let Some(hit) = self.kitty_images.placements().find(|entry| {
            entry.origin == self.kitty_origin
                && entry.wire_image == image_id
                && placement.is_none_or(|pin| entry.wire_placement == pin)
        }) else {
            return false;
        };
        self.kitty_images.get(hit.image).is_some()
    }

    /// Whether a support-probe payload would be admittable for this origin
    /// (S2, #1849).
    ///
    /// Conservative, mutation-free mirror of the store refusal: refuses
    /// only when the global caps would overflow with no eviction at all.
    /// Own-origin pressure is ignored on purpose — a real transmit would
    /// FIFO-evict this origin's oldest images first, so refusing there
    /// would answer `ERROR` where a transmit still succeeds. The fail
    /// direction is deliberate: `OK` always implies admittable, while
    /// `ERROR` may be conservative (never the reverse).
    fn kitty_probe_admittable(&self, decoded_bytes: Option<usize>) -> bool {
        let layer = &self.kitty_images;
        if layer.len().saturating_add(1) > bitty_rich::KITTY_PLACE_MAX_IMAGES {
            return false;
        }
        if let Some(decoded) = decoded_bytes {
            if layer.total_bytes().saturating_add(decoded) > bitty_rich::KITTY_PLACE_MAX_BYTES {
                return false;
            }
        }
        true
    }

    /// Deletes Unicode placeholder grid cells naming `(image_id,
    /// placement_id)` (CTX-0821, issue #1400).
    ///
    /// `placement_id == None` clears every run naming `image_id` (kitty
    /// `a=d,d=i`); `Some(p)` clears only runs naming `(image_id, Some(p))`
    /// (kitty `a=d,d=i,p=`). Only the focused stream's grid
    /// (`self.state`, which `handle_pane_bytes` swaps per pane) is
    /// touched; scrollback history is immutable and keeps its bytes
    /// (dangling cells fail closed: they decode to runs that name
    /// nothing the present layer resolves). Returns the cleared cell
    /// count. A positive count forces a full redraw so the next tick
    /// repaints the cleared cells.
    pub fn kitty_unicode_delete(&mut self, image_id: u32, placement_id: Option<u32>) -> usize {
        let cleared = self.state.kitty_unicode_clear(image_id, placement_id);
        if cleared > 0 {
            self.pending_full_redraw = true;
        }
        cleared
    }

    /// Placeholder runs on one grid row, left-to-right (CTX-0821).
    ///
    /// Headless-observable seam over
    /// [`bitty_term_state::State::kitty_unicode_runs_on_row`]: each entry
    /// is the decoded cells plus the `(image_id, placement_id)` key.
    #[must_use]
    pub fn kitty_unicode_runs_on_row(
        &self,
        row: usize,
    ) -> Vec<bitty_term_state::KittyUnicodeRunCells> {
        self.state.kitty_unicode_runs_on_row(row)
    }
}
