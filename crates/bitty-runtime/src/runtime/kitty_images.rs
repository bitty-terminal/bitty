//! `Runtime` — Kitty graphics display routing (CTX-0248).
//!
//! Split from `super` (`runtime.rs`) as a pure addition: the intake stub
//! ([`bitty_rich::KittyGraphicsStub`]) and the bounded decoder
//! ([`bitty_rich::decode_kitty_payload`]) already exist headlessly, and the
//! VT parser still treats `APC G` as inert, so this module is the minimal
//! routing seam that turns a completed transmission's parameters plus
//! payload bytes into a stored image and, for display actions, a
//! cursor-anchored placement. Full `APC G` parser wiring is follow-up work;
//! callers pass the already-parsed `f`/`s`/`v`/`a`/`c`/`r` values.
//!
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
    /// Bounded decode refused the payload (no bitmap, no placement).
    Decode(bitty_rich::KittyDecodeError),
    /// Placement admission refused an otherwise decoded image.
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
    /// Bound by [`bitty_rich::KITTY_PRESENT_MAX_BLITS_PER_FRAME`].
    #[must_use]
    pub fn kitty_last_frame_images(&self) -> usize {
        self.kitty_last_frame_images
    }

    /// Raster-cache counters: hits, misses, entries, bytes (CTX-0252 F2).
    ///
    /// Headless-observable proof that static frames reuse cached blits
    /// (hits grow, misses do not) and that scroll/geometry changes
    /// invalidate (misses grow, no stale pixels).
    #[must_use]
    pub fn kitty_raster_stats(&self) -> bitty_rich::KittyRasterStats {
        self.kitty_raster_cache.stats()
    }

    /// Decodes `payload` and stores the bitmap without placing it.
    ///
    /// `format_f` is the wire `f=` value (`100` PNG, `24` RGB, `32` RGBA);
    /// `width_s`/`height_v` are the wire `s`/`v` dimensions (required for
    /// raw formats, ignored for PNG). Bounds run before allocation in both
    /// decode and placement admission.
    ///
    /// # Errors
    ///
    /// [`KittyImageError::UnknownFormat`] for unsupported `f=` values,
    /// [`KittyImageError::Decode`] for malformed or oversize payloads, and
    /// [`KittyImageError::Placement`] when the layer refuses admission.
    /// Failures store nothing.
    pub fn kitty_transmit_image(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        payload: &[u8],
        compressed_len: usize,
    ) -> Result<bitty_rich::KittyImageId, KittyImageError> {
        let format = bitty_rich::KittyTransmitFormat::from_f(format_f)
            .ok_or(KittyImageError::UnknownFormat(format_f))?;
        let decoded = bitty_rich::decode_kitty_payload(format, width_s, height_v, payload)
            .map_err(KittyImageError::Decode)?;
        let (width, height) = decoded.dimensions();
        self.kitty_images
            .store(width, height, decoded.into_rgba(), compressed_len)
            .map_err(KittyImageError::Placement)
    }

    /// Decodes, stores, and — for display actions outside the alternate
    /// screen — places a Kitty image at the primary cursor cell.
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
        let compressed_len = payload.len();
        let image =
            self.kitty_transmit_image(format_f, width_s, height_v, payload, compressed_len)?;
        let action = bitty_rich::KittyAction::from_a(action_a);
        if self.state.alt_screen_active() {
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
            .display_for_origin(
                image,
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                rich_metrics,
                self.state.scrollback_len(),
                z,
                self.kitty_origin,
            )
            .map_err(KittyImageError::Placement)?;
        self.pending_full_redraw = true;

        // Per Kitty spec: after placing an image, the cursor moves right by the
        // number of columns and down by the number of rows in the placement
        // rectangle, unless C=1 is explicitly set. Use the effective placement
        // dimensions (what was actually rendered), not the requested spans which
        // may be zero when omitted.
        if cursor_movement_c != 1 {
            // Retrieve the actual placement to get effective dimensions
            let placement = self
                .kitty_images
                .get_placement(placement_id)
                .expect("placement just created must exist");

            let effective_cols = if cols_c > 0 { cols_c } else { placement.cols };
            let effective_rows = if rows_r > 0 { rows_r } else { placement.rows };
            let new_col = cursor.col.saturating_add(effective_cols);
            let new_row = cursor.row.saturating_add(effective_rows);
            self.state.apply(&bitty_vt::TerminalAction::CursorPosition {
                row: bitty_vt::Row(new_row),
                col: bitty_vt::Col(new_col),
            });
        }

        Ok(KittyDisplayOutcome::Displayed {
            image,
            placement: placement_id,
        })
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
