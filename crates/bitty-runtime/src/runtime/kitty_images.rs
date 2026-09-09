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
//! Display anchors at the primary-state cursor cell with
//! `State::scrollback_len()` as the scroll base (images scroll with
//! content; see [`bitty_rich::kitty_place`]). While the alternate screen
//! is active, transmissions decode and store but never place (fail
//! closed, same shape as transmit-only).

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
    /// `z` orders images ascending among themselves.
    ///
    /// A successful display forces a full redraw so the next tick paints
    /// the new placement. Alternate-screen display stores without placing
    /// ([`KittyDisplayOutcome::SuppressedAlternateScreen`]).
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
        let placement = self
            .kitty_images
            .display(
                image,
                cursor.col,
                cursor.row,
                cols_c,
                rows_r,
                rich_metrics,
                self.state.scrollback_len(),
                z,
            )
            .map_err(KittyImageError::Placement)?;
        self.pending_full_redraw = true;
        Ok(KittyDisplayOutcome::Displayed { image, placement })
    }
}
