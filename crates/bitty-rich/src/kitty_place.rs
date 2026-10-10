//! Kitty placement: decoded images onto cell rects (CTX-0248, W-141).
//!
//! [`crate::kitty`] performs intake (chunked `m=` assembly, payloads held
//! inert). The bounded Kitty payload decoder used to live here in
//! `kitty_decode` (W-141 extraction): it now lives in the `bitty-graphics`
//! extension crate, which also owns texture-preparation mechanics
//! (nearest-neighbor scaling, the per-frame blit budget, the raster cache).
//! This module is the Core-retained placement-policy half: it stores
//! caller-supplied decoded bitmaps (validated by [`checked_bitmap`]
//! before admission), binds them to cursor-anchored cell rects, and derives
//! the pixel rects the present layer composites. Parser/APC wiring is
//! unchanged: the caller passes the transmission parameters (`a`, `c`, `r`)
//! alongside the decoded bitmap.
//!
//! Core keeps the pre-allocation checker ([`precheck_declared_image`]) over
//! declared wire dimensions plus the pre-upload re-validator
//! ([`checked_bitmap`]) regardless of the extension-side copies: a
//! repository split never retires a Core check (P0-AC-003/P0-AC-004).
//!
//! # Actions (`a=`)
//!
//! | `a` | Meaning | Effect |
//! |---|---|---|
//! | absent | Kitty default: transmit and display | store + place |
//! | `t` | Transmit only | store, no placement (never painted) |
//! | `T` | Transmit and display | store + place |
//! | anything else (`p` put, `d` delete, `q` query, `f` frame, ...) |
//! | Uncovered | [`KittyAction::Unsupported`]: stored, **not painted**
//! | (fail closed) |
//!
//! [`KittyAction::from_a`] maps the wire value; the layer stores the image
//! in every case and only [`KittyAction::TransmitAndDisplay`] admits a
//! placement. No action deletes, queries, or animates anything here.
//!
//! # Placement semantics
//!
//! - Anchor: the cursor cell (`anchor_col`, `anchor_row`) at display time.
//! - Size: explicit `c`/`r` cell spans when non-zero, otherwise derived from
//!   decoded pixels (`ceil(px / cell)`, at least 1 cell per axis).
//! - The cell rect maps to pixels through the caller-supplied
//!   [`crate::geometry::CellMetrics`] and is clamped (intersected) to the
//!   viewport; a placement fully outside the viewport paints nothing.
//! - Z-order (documented model): grid cells (backgrounds + glyphs) and every
//!   fill overlay (selection, cursor, banner, scrollbar) composite first;
//!   kitty images are the **topmost** present-layer content, ordered by
//!   ascending `z` (stable for equal `z`). They never mutate grid truth.
//!   Known limitation: an image covering the cursor or selection cell hides
//!   that fill where they overlap; cursor-on-top is follow-up work.
//!
//! # Scroll (tied to grid rows)
//!
//! Each placement records `scrollback_base` (`State::scrollback_len()` at
//! display time). At present time the scrolled distance is
//! `current.saturating_sub(base)`; the effective anchor row is
//! `anchor_row - scrolled` (may be negative: the top edge is clipped), and
//! only placements scrolled *fully* off the top paint nothing. The image therefore moves **with** terminal content. Viewport
//! scrollback inspection (`View::scroll_offset() != 0`) is handled by the
//! caller (skip painting: live-grid anchors do not map to the history
//! viewport), as is alternate-screen suppression (see below).
//!
//! Limitation: scroll regions (`DECSTBM`) and reverse-index moves that do
//! not grow the scrollback are not tracked, so an image inside a scroll
//! region may lag its text by the region-local delta. Full-screen scroll
//! (the common case) is exact.
//!
//! # Alternate screen
//!
//! [`KittyImageLayer::clear`] drops every image and placement. The caller
//! clears when the alternate screen activates and refuses new placements
//! while it is active (stored, not painted — same fail-closed shape as
//! [`KittyAction::Transmit`]).
//!
//! # Origin binding + spoofing posture (CTX-0254)
//!
//! Every placement carries an [`KittyPlacement::origin`] token identifying
//! the PTY stream that emitted it: `None` for the primary grid, `Some(id)`
//! for a split-pane session (the runtime passes the leaf's `ViewId.0`; this
//! crate stays `u64`-typed so it never depends on the UI crate). The
//! present layer paints only the focused leaf's origin on that leaf, so a
//! background pane's program can never paint pixels over the focused pane's
//! grid (cross-pane spoof prevention). Scrollback and alternate-screen
//! state are likewise resolved per origin at present time, never against a
//! global grid.
//!
//! Residual posture (documented, not enforced here):
//!
//! - Within one origin, images stay topmost over that pane's own cursor and
//!   selection fills (see "Placement semantics" above). Same-origin impact
//!   is contained: the emitting program already controls every cell of its
//!   own grid, so covering its own chrome adds no new spoof capability
//!   beyond what PTY text already allows. Cursor-on-top remains follow-up
//!   work.
//! - The decoded-image *store* stays global and bounded (FIFO eviction on
//!   the count/byte caps). A noisy origin can therefore evict another
//!   origin's stored images (availability only — dangling placements fail
//!   closed at lookup and paint nothing). Per-origin quotas are follow-up
//!   work if this ever matters operationally.
//!
//! # Bounds (threat T-01/T-02)
//!
//! Placement enforces the Core-retained decode ceilings
//! ([`KITTY_DECODE_MAX_DIMENSION`]/[`KITTY_DECODE_MAX_PIXELS`]/[`KITTY_DECODE_MAX_BYTES`]:
//! 8192 px/side, 4096 x 4096 px area, 64 MiB RGBA) rather than
//! inventing its own. These are Core policy constants mirroring the
//! versioned graphics contract; the `bitty-graphics` extension holds its
//! own copies and receives limits per request, never via a Core import
//! (one-way dependencies). Every
//! length is validated with checked arithmetic **before** any buffer is
//! allocated or grown, including the viewport-clamped blit size
//! (`rect_w * rect_h * 4`, itself bounded because the rect is clamped to
//! the viewport first). Layer totals are additionally capped: 64 stored
//! images ([`KITTY_PLACE_MAX_IMAGES`], kitty-ledger parity) and 256 MiB
//! decoded bytes ([`KITTY_PLACE_MAX_BYTES`], RFC IMG-4 parity), oldest
//! evicted first; placements are capped at 128 ([`KITTY_PLACE_MAX_ITEMS`],
//! RFC IMG-8 parity), oldest evicted first. A single image larger than the
//! byte cap is rejected.
//!
//! # Per-frame budget (CTX-0252 F2, policy half retained)
//!
//! The present loop composites at most [`KITTY_PRESENT_MAX_BLITS_PER_FRAME`]
//! blits / [`KITTY_PRESENT_MAX_BYTES_PER_FRAME`] bytes per frame
//! (skip-and-continue in paint order), so the pathological 128-placement
//! transient (128 x 64 MiB) can never materialize. The frame-budget
//! accounting type and the scaled-blit raster cache moved to the
//! `bitty-graphics` extension with the rest of the texture-preparation
//! mechanics; the caps above stay Core-owned so the present layer keeps
//! enforcing the same ceilings the parity tests pin.
//!
//! # Determinism
//!
//! Storage and placement are pure functions of insertion order: same calls
//! always yield the same ids and the same retained set.

use std::collections::VecDeque;

use crate::geometry::{CellMetrics, ExtentPx, RectPx};

/// Kitty `f=` value for PNG payloads (Core-retained contract mirror).
///
/// The bounded decoder moved to the `bitty-graphics` extension (W-141),
/// which holds its own copy. Core keeps this mirror so wire-format
/// admission (`UnknownFormat` mapping) and declared-size pre-checks stay
/// Core-owned; the two sides must not drift (same values, per-request
/// carriage, never a cross-crate import).
pub const KITTY_FORMAT_PNG: u32 = 100;
/// Kitty `f=` value for raw 24-bit RGB payloads (Core-retained mirror, see
/// [`KITTY_FORMAT_PNG`]).
pub const KITTY_FORMAT_RGB: u32 = 24;
/// Kitty `f=` value for raw 32-bit RGBA payloads (Core-retained mirror,
/// see [`KITTY_FORMAT_PNG`]).
pub const KITTY_FORMAT_RGBA: u32 = 32;

/// Maximum decoded image width or height in pixels (Core-retained
/// pre-allocation/re-validation ceiling, mirrors the versioned graphics
/// contract; the extension holds its own copy).
pub const KITTY_DECODE_MAX_DIMENSION: u32 = 8192;
/// Maximum decoded pixels, 4096 x 4096 area (Core-retained ceiling, see
/// [`KITTY_DECODE_MAX_DIMENSION`]).
///
/// Couples the two per-side ceilings into one area bound so a decoded RGBA8
/// bitmap never exceeds [`KITTY_DECODE_MAX_BYTES`]. Wide aspect ratios up to
/// [`KITTY_DECODE_MAX_DIMENSION`] per side still pass (for example
/// 8192 x 2048); anything denser than the accepted RFC frame is rejected
/// before allocation.
pub const KITTY_DECODE_MAX_PIXELS: u64 = 4096 * 4096;
/// Maximum decoded RGBA8 bytes, mirrors `IMG-3` (Core-retained ceiling,
/// see [`KITTY_DECODE_MAX_DIMENSION`]).
pub const KITTY_DECODE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Maximum image blits composited in one present frame (Core-retained
/// present-policy ceiling; the accounting type moved to the extension).
pub const KITTY_PRESENT_MAX_BLITS_PER_FRAME: usize = 32;
/// Maximum scaled blit bytes composited in one present frame, 64 MiB
/// (Core-retained present-policy ceiling; the accounting type moved to the
/// extension).
pub const KITTY_PRESENT_MAX_BYTES_PER_FRAME: usize = 64 * 1024 * 1024;

/// Maximum stored decoded images (kitty-ledger count parity).
pub const KITTY_PLACE_MAX_IMAGES: usize = crate::kitty::KITTY_MAX_PLACEHOLDERS;

/// Maximum total decoded RGBA bytes held by the layer (RFC IMG-4 parity).
pub const KITTY_PLACE_MAX_BYTES: usize = crate::image::IMAGE_STORE_MAX_BYTES;

/// Maximum placements (RFC IMG-8 parity).
pub const KITTY_PLACE_MAX_ITEMS: usize = crate::image::IMAGE_MAX_PLACEMENTS;

/// Maximum scroll lines one Kitty placement may drive (CTX-1072, #1850).
///
/// The wire `r=` span is untrusted PTY input: a maximal-row (`r=65535`)
/// placement of a 1x1 image must not drive about 65k linefeeds per
/// placement. The runtime caps cursor-advance scroll to the trusted
/// viewport height and to this absolute ceiling, whichever is smaller,
/// so per-placement scroll work stays bounded while in-budget placements
/// (a few rows) behave exactly as before. Mirrors the per-frame blit
/// budget posture (`KITTY_PRESENT_MAX_*`): a named Core-owned ceiling
/// enforced at the call site, no unbounded PTY-driven loops.
pub const KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT: u16 = 256;

// ---------------------------------------------------------------------------
// Declared-size pre-check (Core-retained, P0-AC-003)
// ---------------------------------------------------------------------------

/// Typed declared-size pre-check rejection.
///
/// Returned by [`precheck_declared_image`] before any pixel buffer exists.
/// The transmit seam maps the [`crate::kitty_decode`] codec failures into
/// this same taxonomy so refusal behavior (and log greps) stay stable
/// across the pre-check and decode stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyPrecheckError {
    /// Empty payload carries no image.
    EmptyPayload,
    /// Raw RGB/RGBA arrived without both `s` (width) and `v` (height).
    MissingDimensions,
    /// A declared dimension is zero.
    ZeroDimension,
    /// A declared dimension exceeds [`KITTY_DECODE_MAX_DIMENSION`];
    /// rejected before any pixel buffer exists.
    DimensionsTooLarge {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// Side cap that refused them.
        cap: u32,
    },
    /// `width * height` exceeds [`KITTY_DECODE_MAX_PIXELS`]; rejected
    /// before any pixel buffer exists.
    TooManyPixels {
        /// Declared pixel count.
        pixels: u64,
        /// Area cap that refused it.
        cap: u64,
    },
    /// Declared bytes exceed [`KITTY_DECODE_MAX_BYTES`]; rejected before
    /// the pixel buffer is allocated.
    DecodedTooLarge {
        /// Bytes the bitmap would have needed (`usize::MAX` when the size
        /// computation itself overflowed).
        bytes: usize,
        /// Byte cap that refused them.
        cap: usize,
    },
    /// Raw payload length is not exactly `width * height * channels`.
    LengthMismatch {
        /// `width * height * channels`.
        expected: usize,
        /// Actual payload length.
        actual: usize,
    },
    /// The PNG stream is malformed, truncated, or undecodable; carries the
    /// decoder diagnostic. Same bytes always produce the same message.
    MalformedPng(String),
    /// Legacy W-141 stub variant (kept for API stability; no longer
    /// constructed now that Core owns decode in [`crate::kitty_decode`]).
    DecoderUnavailable,
}

impl std::fmt::Display for KittyPrecheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPayload => write!(f, "kitty payload is empty"),
            Self::MissingDimensions => {
                write!(f, "kitty raw payload needs width and height (s/v)")
            }
            Self::ZeroDimension => write!(f, "kitty image dimension is zero"),
            Self::DimensionsTooLarge { width, height, cap } => write!(
                f,
                "kitty image {width}x{height} exceeds max dimension of {cap}px"
            ),
            Self::TooManyPixels { pixels, cap } => write!(
                f,
                "kitty image of {pixels} pixels exceeds max of {cap} pixels"
            ),
            Self::DecodedTooLarge { bytes, cap } => write!(
                f,
                "kitty decoded bitmap of {bytes} bytes exceeds max of {cap} bytes"
            ),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "kitty raw payload of {actual} bytes does not match {expected} expected bytes"
            ),
            Self::MalformedPng(detail) => write!(f, "kitty PNG is malformed: {detail}"),
            Self::DecoderUnavailable => write!(
                f,
                "kitty decoder unavailable (legacy stub; Core now decodes)"
            ),
        }
    }
}

impl std::error::Error for KittyPrecheckError {}

/// Maps a Core-owned decode failure into the transmit-seam taxonomy.
///
/// The variants mirror each other 1:1 except
/// [`crate::kitty_decode::KittyDecodeError`] transport for unknown `f=`
/// values, which the caller rejects as `UnknownFormat` before decoding.
impl From<crate::kitty_decode::KittyDecodeError> for KittyPrecheckError {
    fn from(err: crate::kitty_decode::KittyDecodeError) -> Self {
        use crate::kitty_decode::KittyDecodeError as D;
        match err {
            D::EmptyPayload => Self::EmptyPayload,
            D::MissingDimensions => Self::MissingDimensions,
            D::ZeroDimension => Self::ZeroDimension,
            D::DimensionsTooLarge { width, height, cap } => {
                Self::DimensionsTooLarge { width, height, cap }
            }
            D::TooManyPixels { pixels, cap } => Self::TooManyPixels { pixels, cap },
            D::DecodedTooLarge { bytes, cap } => Self::DecodedTooLarge { bytes, cap },
            D::LengthMismatch { expected, actual } => Self::LengthMismatch { expected, actual },
            D::MalformedPng(detail) => Self::MalformedPng(detail),
        }
    }
}

/// Validates declared wire dimensions with checked arithmetic before any
/// allocation.
///
/// Returns the pixel count. Zero, over-side, and over-area inputs are
/// rejected here so no caller can allocate from untrusted dimensions. This
/// is the Core-retained counterpart of the moved decoder's header check.
fn checked_declared_dimensions(width: u32, height: u32) -> Result<u64, KittyPrecheckError> {
    if width == 0 || height == 0 {
        return Err(KittyPrecheckError::ZeroDimension);
    }
    if width > KITTY_DECODE_MAX_DIMENSION || height > KITTY_DECODE_MAX_DIMENSION {
        return Err(KittyPrecheckError::DimensionsTooLarge {
            width,
            height,
            cap: KITTY_DECODE_MAX_DIMENSION,
        });
    }
    // No overflow is possible: both sides are at most 8192.
    let pixels = u64::from(width) * u64::from(height);
    if pixels > KITTY_DECODE_MAX_PIXELS {
        return Err(KittyPrecheckError::TooManyPixels {
            pixels,
            cap: KITTY_DECODE_MAX_PIXELS,
        });
    }
    Ok(pixels)
}

/// Pre-allocation pre-check over a declared Kitty transmission.
///
/// `channels` is `None` for PNG (declared `s`/`v` ignored: the `IHDR`
/// governs and the extension enforces it) and `Some(3)`/`Some(4)` for raw
/// RGB/RGBA (both dimensions required, exact-length enforced). Every
/// refusal happens before any pixel buffer is allocated (P0-AC-003).
/// Success means only that the declaration is admissible: producing the
/// bitmap is the extension's job.
///
/// # Errors
///
/// [`KittyPrecheckError`] variants for empty, underspecified, oversize, or
/// length-mismatched declarations. Failures admit nothing.
pub fn precheck_declared_image(
    channels: Option<usize>,
    width: Option<u32>,
    height: Option<u32>,
    payload_len: usize,
) -> Result<(), KittyPrecheckError> {
    if payload_len == 0 {
        return Err(KittyPrecheckError::EmptyPayload);
    }
    let Some(channels) = channels else {
        // PNG: declared dimensions are meaningless; the extension validates
        // the IHDR before allocating.
        return Ok(());
    };
    let (Some(w), Some(h)) = (width, height) else {
        return Err(KittyPrecheckError::MissingDimensions);
    };
    let pixels = checked_declared_dimensions(w, h)?;
    let expected = (pixels as usize)
        .checked_mul(channels)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyPrecheckError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    if payload_len != expected {
        return Err(KittyPrecheckError::LengthMismatch {
            expected,
            actual: payload_len,
        });
    }
    // The admitted declaration expands to `pixels * 4 <=
    // KITTY_DECODE_MAX_BYTES`: `pixels <= KITTY_DECODE_MAX_PIXELS`
    // (4096^2) bounds the RGBA expansion identically to the moved decoder.
    (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyPrecheckError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Kitty `a=` display action (subset implemented for CTX-0248).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyAction {
    /// `a=t`: transmit only — store, never place.
    Transmit,
    /// Absent `a` (kitty default) or `a=T`: store and place.
    TransmitAndDisplay,
    /// Any other `a=` value: stored, **not painted** (fail closed).
    Unsupported(char),
}

impl KittyAction {
    /// Maps the wire `a=` value (`None` when the parameter is absent).
    ///
    /// Absent means transmit-and-display per the kitty specification.
    #[must_use]
    pub const fn from_a(value: Option<char>) -> Self {
        match value {
            None | Some('T') => Self::TransmitAndDisplay,
            Some('t') => Self::Transmit,
            Some(other) => Self::Unsupported(other),
        }
    }

    /// Whether this action admits a placement (paints).
    #[must_use]
    pub const fn displays(self) -> bool {
        matches!(self, Self::TransmitAndDisplay)
    }
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Stable handle for a stored decoded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KittyImageId(pub u64);

impl KittyImageId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Stable handle for a placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KittyPlacementId(pub u64);

impl KittyPlacementId {
    /// Numeric value for diagnostics only.
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Stored decoded image: owned RGBA8 pixels in row-major order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacedImage {
    /// Stable handle.
    pub id: KittyImageId,
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// RGBA8 bytes, exactly `width * height * 4` long.
    pub rgba: Vec<u8>,
    /// Wire payload length (diagnostics; the bytes themselves are decoded).
    pub compressed_len: usize,
}

/// Placement binding one image to a cursor-anchored cell rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacement {
    /// Stable handle.
    pub id: KittyPlacementId,
    /// Which image is placed.
    pub image: KittyImageId,
    /// Origin token of the emitting PTY stream (CTX-0254): `None` for the
    /// primary grid, `Some(token)` for a split-pane session (the runtime
    /// passes the leaf's `ViewId.0`). The present layer paints a placement
    /// only on its own origin's leaf, so background panes cannot spoof
    /// pixels over the focused pane.
    pub origin: Option<u64>,
    /// Wire `i=` image id naming this placement (CTX-1072, #1850): `0`
    /// when absent (anonymous). Together with `origin` and
    /// `wire_placement` this is the origin-scoped identity mapping that
    /// lets protocol-level deletion (`a=d,d=i`) reliably clear the
    /// rendered placement instead of leaving an orphan blit.
    pub wire_image: u32,
    /// Wire `p=` placement id naming this placement (CTX-1072, #1850):
    /// `0` when absent (anonymous, never singly addressable, mirroring
    /// the terminal-truth store).
    pub wire_placement: u32,
    /// Cursor column at display time.
    pub anchor_col: u16,
    /// Cursor row at display time (before scroll adjustment).
    pub anchor_row: u16,
    /// Width in cells (explicit `c=` or derived, always `>= 1`).
    pub cols: u16,
    /// Height in cells (explicit `r=` or derived, always `>= 1`).
    pub rows: u16,
    /// `State::scrollback_len()` at display time (scroll tracking).
    pub scrollback_base: usize,
    /// Stacking order (ascending paints first).
    pub z: i32,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed placement rejection.
///
/// Every variant fails closed: the layer keeps its previous state except
/// where documented (FIFO eviction on admission success only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyPlacementError {
    /// A side exceeds 8192 px; rejected before any allocation.
    DimensionsTooLarge {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
        /// Side cap that refused them.
        cap: u32,
    },
    /// `width * height` exceeds 4096 x 4096 px; rejected before allocation.
    TooManyPixels {
        /// Requested pixel count.
        pixels: u64,
        /// Area cap that refused it.
        cap: u64,
    },
    /// RGBA bytes exceed 64 MiB; rejected before allocation (or growth).
    DecodedTooLarge {
        /// Bytes the bitmap would have needed (`usize::MAX` on overflow).
        bytes: usize,
        /// Byte cap that refused them.
        cap: usize,
    },
    /// RGBA length is not exactly `width * height * 4`.
    LengthMismatch {
        /// `width * height * 4`.
        expected: usize,
        /// Actual length.
        actual: usize,
    },
    /// Placement references an unknown (or evicted) image.
    ImageNotFound(KittyImageId),
}

impl std::fmt::Display for KittyPlacementError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DimensionsTooLarge { width, height, cap } => write!(
                f,
                "kitty placement {width}x{height} exceeds max dimension of {cap}px"
            ),
            Self::TooManyPixels { pixels, cap } => write!(
                f,
                "kitty placement of {pixels} pixels exceeds max of {cap} pixels"
            ),
            Self::DecodedTooLarge { bytes, cap } => write!(
                f,
                "kitty placement bitmap of {bytes} bytes exceeds max of {cap} bytes"
            ),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "kitty placement rgba of {actual} bytes does not match {expected} expected bytes"
            ),
            Self::ImageNotFound(id) => write!(f, "kitty placement image not found: {}", id.0),
        }
    }
}

impl std::error::Error for KittyPlacementError {}

// ---------------------------------------------------------------------------
// Validation (no allocation)
// ---------------------------------------------------------------------------

/// Validates `(width, height, rgba.len())` with checked arithmetic.
///
/// Returns the pixel count. Runs before any buffer exists or grows, so
/// hostile dimensions can never force an over-cap allocation.
fn checked_bitmap(width: u32, height: u32, rgba_len: usize) -> Result<u64, KittyPlacementError> {
    if width == 0 || height == 0 {
        // Zero-dimension bitmaps carry no pixels; report deterministically
        // through the exact-length check (expected 0 vs actual).
        return Err(KittyPlacementError::LengthMismatch {
            expected: 0,
            actual: rgba_len,
        });
    }
    if width > KITTY_DECODE_MAX_DIMENSION || height > KITTY_DECODE_MAX_DIMENSION {
        return Err(KittyPlacementError::DimensionsTooLarge {
            width,
            height,
            cap: KITTY_DECODE_MAX_DIMENSION,
        });
    }
    // No overflow is possible: both sides are at most 8192.
    let pixels = u64::from(width) * u64::from(height);
    if pixels > KITTY_DECODE_MAX_PIXELS {
        return Err(KittyPlacementError::TooManyPixels {
            pixels,
            cap: KITTY_DECODE_MAX_PIXELS,
        });
    }
    let expected = (pixels as usize)
        .checked_mul(4)
        .filter(|&n| n <= KITTY_DECODE_MAX_BYTES)
        .ok_or(KittyPlacementError::DecodedTooLarge {
            bytes: usize::MAX,
            cap: KITTY_DECODE_MAX_BYTES,
        })?;
    if rgba_len != expected {
        return Err(KittyPlacementError::LengthMismatch {
            expected,
            actual: rgba_len,
        });
    }
    Ok(pixels)
}

// ---------------------------------------------------------------------------
// Cell-span derivation
// ---------------------------------------------------------------------------

/// Cell span for one axis: explicit `c=`/`r=` when non-zero, otherwise
/// `ceil(pixels / cell_px)` (at least 1 cell).
///
/// `cell_px` is guaranteed non-zero by [`CellMetrics`] construction.
fn cell_span(explicit: u16, pixels: u32, cell_px: u32) -> u16 {
    if explicit != 0 {
        return explicit;
    }
    let cells = pixels.div_ceil(cell_px).max(1);
    cells.min(u16::MAX as u32) as u16
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// Owned Kitty image + placement layer (headless, bounded).
///
/// Deterministic FIFO eviction on images (count and byte caps) and on
/// placements (count cap). Same call order always yields the same ids and
/// the same retained set.
#[derive(Debug, Clone, Default)]
pub struct KittyImageLayer {
    images: VecDeque<KittyPlacedImage>,
    placements: VecDeque<KittyPlacement>,
    total_bytes: usize,
    next_image_id: u64,
    next_placement_id: u64,
}

impl KittyImageLayer {
    /// An empty layer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            images: VecDeque::new(),
            placements: VecDeque::new(),
            total_bytes: 0,
            next_image_id: 1,
            next_placement_id: 1,
        }
    }

    /// Number of stored decoded images.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether no decoded image is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Decoded RGBA bytes currently held.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Number of retained placements.
    #[must_use]
    pub fn placement_len(&self) -> usize {
        self.placements.len()
    }

    /// Whether no placement is retained.
    #[must_use]
    pub fn placement_is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Stores a decoded bitmap, validating decode caps before admission.
    ///
    /// `rgba` must be exactly `width * height * 4` bytes. On success the
    /// oldest images are evicted first to satisfy the count and byte caps
    /// (placements of evicted images are dropped deterministically).
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError`] dimension/area/byte/length rejections, or
    /// [`KittyPlacementError::DecodedTooLarge`] when the bitmap alone
    /// exceeds the layer byte cap. Failures store nothing.
    pub fn store(
        &mut self,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed_len: usize,
    ) -> Result<KittyImageId, KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        let bytes = rgba.len();
        if bytes > KITTY_PLACE_MAX_BYTES {
            return Err(KittyPlacementError::DecodedTooLarge {
                bytes,
                cap: KITTY_PLACE_MAX_BYTES,
            });
        }
        while self.images.len() >= KITTY_PLACE_MAX_IMAGES
            || self.total_bytes.saturating_add(bytes) > KITTY_PLACE_MAX_BYTES
        {
            if let Some(evicted) = self.images.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.rgba.len());
                let evicted_id = evicted.id;
                self.placements.retain(|p| p.image != evicted_id);
            } else {
                break;
            }
        }
        let id = KittyImageId(self.next_image_id);
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        self.images.push_back(KittyPlacedImage {
            id,
            width,
            height,
            rgba,
            compressed_len,
        });
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        Ok(id)
    }

    /// Looks up a stored image by id.
    #[must_use]
    pub fn get(&self, id: KittyImageId) -> Option<&KittyPlacedImage> {
        self.images.iter().find(|img| img.id == id)
    }

    /// Removes the image with `id` and its placements; `true` when removed.
    pub fn remove(&mut self, id: KittyImageId) -> bool {
        let before = self.images.len();
        let mut removed_bytes = 0usize;
        self.images.retain(|img| {
            if img.id == id {
                removed_bytes = removed_bytes.saturating_add(img.rgba.len());
                false
            } else {
                true
            }
        });
        let removed = self.images.len() != before;
        if removed {
            self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
            self.placements.retain(|p| p.image != id);
        }
        removed
    }

    /// Places a stored image at the cursor cell.
    ///
    /// `cols`/`rows` are the explicit `c=`/`r=` spans (0 or absent derives
    /// from decoded pixels). `scrollback_base` is
    /// `State::scrollback_len()` now. Evicts the oldest placement at the
    /// 128 cap (FIFO). The placement is bound to the primary origin
    /// (`None`); pane sessions use [`KittyImageLayer::display_for_origin`].
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] when `image` is unknown.
    /// Stores nothing new on failure.
    #[allow(clippy::too_many_arguments)]
    pub fn display(
        &mut self,
        image: KittyImageId,
        anchor_col: u16,
        anchor_row: u16,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        self.display_for_origin(
            image,
            anchor_col,
            anchor_row,
            cols,
            rows,
            metrics,
            scrollback_base,
            z,
            None,
        )
    }

    /// Places a stored image at the cursor cell for one origin (CTX-0254).
    ///
    /// Identical to [`KittyImageLayer::display`] except the placement is
    /// tagged with `origin` (`None` primary, `Some(token)` pane session),
    /// so the present layer can confine it to its own leaf. Same errors
    /// and eviction behavior as [`KittyImageLayer::display`]. The wire
    /// identity defaults to anonymous (`0`, `0`); callers with protocol
    /// ids use [`KittyImageLayer::display_for_origin_with_wire`].
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] when `image` is unknown.
    /// Stores nothing new on failure.
    #[allow(clippy::too_many_arguments)]
    pub fn display_for_origin(
        &mut self,
        image: KittyImageId,
        anchor_col: u16,
        anchor_row: u16,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
        origin: Option<u64>,
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        self.display_for_origin_with_wire(
            image,
            anchor_col,
            anchor_row,
            cols,
            rows,
            metrics,
            scrollback_base,
            z,
            origin,
            0,
            0,
        )
    }

    /// Places a stored image with origin-scoped wire identity (CTX-1072).
    ///
    /// Identical to [`KittyImageLayer::display_for_origin`] except the
    /// placement also records the wire `i=`/`p=` ids (`0` when absent).
    /// Protocol-level deletion (`a=d,d=i`) matches on
    /// `(origin, wire_image, wire_placement)` via
    /// [`KittyImageLayer::delete_by_wire`], so the rendered placement
    /// clears instead of leaving an orphan blit. Anonymous placements
    /// (`wire_placement == 0`) are never singly addressable, mirroring
    /// the terminal-truth store.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] when `image` is unknown.
    /// Stores nothing new on failure.
    #[allow(clippy::too_many_arguments)]
    pub fn display_for_origin_with_wire(
        &mut self,
        image: KittyImageId,
        anchor_col: u16,
        anchor_row: u16,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        let stored = self
            .get(image)
            .ok_or(KittyPlacementError::ImageNotFound(image))?;
        let cols = cell_span(cols, stored.width, metrics.width);
        let rows = cell_span(rows, stored.height, metrics.height);
        if self.placements.len() >= KITTY_PLACE_MAX_ITEMS {
            self.placements.pop_front();
        }
        let id = KittyPlacementId(self.next_placement_id);
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        self.placements.push_back(KittyPlacement {
            id,
            image,
            origin,
            wire_image,
            wire_placement,
            anchor_col,
            anchor_row,
            cols,
            rows,
            scrollback_base,
            z,
        });
        Ok(id)
    }

    /// Looks up a placement by id.
    #[must_use]
    pub fn get_placement(&self, id: KittyPlacementId) -> Option<&KittyPlacement> {
        self.placements.iter().find(|p| p.id == id)
    }

    /// Removes a placement by id.
    pub fn remove_placement(&mut self, id: KittyPlacementId) -> bool {
        let before = self.placements.len();
        self.placements.retain(|p| p.id != id);
        self.placements.len() != before
    }

    /// Iterates placements oldest first.
    pub fn placements(&self) -> impl Iterator<Item = &KittyPlacement> {
        self.placements.iter()
    }

    /// Placements in paint order: ascending `z`, stable for equal `z`.
    pub fn placements_in_paint_order(&self) -> Vec<&KittyPlacement> {
        let mut ordered: Vec<&KittyPlacement> = self.placements.iter().collect();
        ordered.sort_by_key(|p| p.z);
        ordered
    }

    /// Placements of one origin in paint order (CTX-0254).
    ///
    /// The present layer paints only the focused leaf's origin, so a
    /// background pane's placements never reach another leaf's frame.
    /// Ascending `z`, stable for equal `z`, like
    /// [`KittyImageLayer::placements_in_paint_order`].
    pub fn placements_in_paint_order_for(&self, origin: Option<u64>) -> Vec<&KittyPlacement> {
        let mut ordered: Vec<&KittyPlacement> = self
            .placements
            .iter()
            .filter(|p| p.origin == origin)
            .collect();
        ordered.sort_by_key(|p| p.z);
        ordered
    }

    /// Whether any placement of `origin` is retained.
    #[must_use]
    pub fn placement_for_origin_is_empty(&self, origin: Option<u64>) -> bool {
        !self.placements.iter().any(|p| p.origin == origin)
    }

    /// Drops every placement of `origin`, keeping other origins and all
    /// stored images (CTX-0254).
    ///
    /// Alternate-screen entry calls this for the entering origin only, so
    /// one pane's fullscreen app never wipes another pane's images.
    /// Images left without placements stay inert (never painted) and age
    /// out under the store caps.
    pub fn clear_origin(&mut self, origin: Option<u64>) {
        self.placements.retain(|p| p.origin != origin);
    }

    /// Deletes rendered placements by origin-scoped wire identity (CTX-1072).
    ///
    /// Matches the `a=d,d=i` protocol selector: every placement with
    /// `origin` and `wire_image == image_id` is removed, or only the one
    /// with `wire_placement == placement` when `placement` is `Some`.
    /// Anonymous placements (`wire_placement == 0`) never match a pinned
    /// delete, mirroring the terminal-truth store. Other origins are
    /// untouched, stored images stay inert under the store caps, and the
    /// return is the removed count. Location-based selectors
    /// (`c`/`p`/`q`/`r`/`x`/`y`/`z`) stay follow-up work: they address
    /// screen geometry, not identity, so they cannot orphan through this
    /// mapping.
    pub fn delete_by_wire(
        &mut self,
        origin: Option<u64>,
        image_id: u32,
        placement: Option<u32>,
    ) -> usize {
        let before = self.placements.len();
        match placement {
            Some(pinned) if pinned != 0 => {
                self.placements.retain(|p| {
                    !(p.origin == origin && p.wire_image == image_id && p.wire_placement == pinned)
                });
            }
            Some(_) => {
                // Pin `0` names nothing singly addressable: remove nothing.
            }
            None => {
                self.placements
                    .retain(|p| !(p.origin == origin && p.wire_image == image_id));
            }
        }
        before - self.placements.len()
    }

    /// Clears all images and placements (alternate-screen entry).
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.total_bytes = 0;
    }

    /// Scroll-adjusted, unclamped pixel rect for a placement.
    ///
    /// Same anchor/scroll math as [`KittyImageLayer::placement_rect`] but
    /// without the viewport intersection: the full cell-rect extent the
    /// image scales into, even where it overflows the viewport. Returns
    /// `None` only when the placement scrolled fully off the top.
    #[must_use]
    pub fn placement_full_rect(
        placement: &KittyPlacement,
        metrics: CellMetrics,
        scrollback_now: usize,
    ) -> Option<RectPx> {
        let scrolled = scrollback_now.saturating_sub(placement.scrollback_base);
        let scrolled_i64 = i64::try_from(scrolled).unwrap_or(i64::MAX);
        let anchor_row = i64::from(placement.anchor_row);
        let rows = i64::from(placement.rows);
        let bottom_row = anchor_row.saturating_add(rows).saturating_sub(scrolled_i64);
        if bottom_row <= 0 {
            return None;
        }
        let row_offset = anchor_row.saturating_sub(scrolled_i64);
        let x = u64::from(placement.anchor_col) * u64::from(metrics.width);
        let y = row_offset.saturating_mul(i64::from(metrics.height));
        let w = u64::from(placement.cols) * u64::from(metrics.width);
        let h = u64::from(placement.rows) * u64::from(metrics.height);
        Some(RectPx::new(
            saturating_i32(x),
            saturating_i64_to_i32(y),
            saturating_u32(w),
            saturating_u32(h),
        ))
    }

    /// Pixel rect for a placement in the current viewport, if visible.
    ///
    /// `viewport_cols`/`viewport_rows` are the live grid dimensions;
    /// `scrollback_now` is the current `State::scrollback_len()`. Returns
    /// `None` when the placement scrolled fully off the top or lies fully
    /// outside the viewport (paints nothing). Otherwise returns the
    /// full rect ([`KittyImageLayer::placement_full_rect`]) intersected
    /// with the viewport. The scaler must crop the scaled image to this
    /// rect (the extension's clipped rasterizer), never re-scale the whole
    /// source into it: re-scaling squeezes a partially visible image
    /// instead of cropping it (#1334 first-paint squash).
    #[must_use]
    pub fn placement_rect(
        placement: &KittyPlacement,
        metrics: CellMetrics,
        viewport_cols: u16,
        viewport_rows: u16,
        scrollback_now: usize,
    ) -> Option<RectPx> {
        let full = Self::placement_full_rect(placement, metrics, scrollback_now)?;
        // Viewport extent in pixels.
        let extent = metrics.extent_for(usize::from(viewport_cols), usize::from(viewport_rows));
        intersect(RectPx::new(0, 0, extent.width, extent.height), full)
    }
}

/// Pixel rect for a placement against this layer's stored image, if visible.
///
/// Convenience wrapper over [`KittyImageLayer::placement_rect`] that
/// resolves the id first (`None` for unknown placements).
#[must_use]
pub fn placement_rect_for(
    layer: &KittyImageLayer,
    id: KittyPlacementId,
    metrics: CellMetrics,
    viewport_cols: u16,
    viewport_rows: u16,
    scrollback_now: usize,
) -> Option<RectPx> {
    let placement = layer.get_placement(id)?;
    KittyImageLayer::placement_rect(
        placement,
        metrics,
        viewport_cols,
        viewport_rows,
        scrollback_now,
    )
}

/// Scroll-adjusted, unclamped pixel rect for a placement, if retained.
///
/// Convenience wrapper over [`KittyImageLayer::placement_full_rect`]
/// that resolves the id first (`None` for unknown placements, or when
/// the placement scrolled fully off the top).
#[must_use]
pub fn placement_full_rect_for(
    layer: &KittyImageLayer,
    id: KittyPlacementId,
    metrics: CellMetrics,
    scrollback_now: usize,
) -> Option<RectPx> {
    let placement = layer.get_placement(id)?;
    KittyImageLayer::placement_full_rect(placement, metrics, scrollback_now)
}

// ---------------------------------------------------------------------------
// Texture-preparation mechanics (moved to `bitty-graphics`, W-141)
// ---------------------------------------------------------------------------

// Nearest-neighbor scaling (`rasterize`, `rasterize_clipped`), the
// per-frame blit budget (`KittyFrameBudget`), and the bounded raster cache
// (`KittyRasterKey`/`KittyRasterStats`/`KittyRasterCache` with the
// `KITTY_RASTER_CACHE_MAX_*` caps) moved to the `bitty-graphics` extension
// crate. The ceilings stay Core-owned above (`KITTY_PRESENT_MAX_*`) so the
// present layer keeps enforcing identical bounds; pixel production awaits
// the Core-to-extension call shape (not wired yet).

// ---------------------------------------------------------------------------
// Small integer helpers (mirror the grid pipeline's saturating style)
// ---------------------------------------------------------------------------

const fn saturating_i32(value: u64) -> i32 {
    if value > i32::MAX as u64 {
        i32::MAX
    } else {
        value as i32
    }
}

const fn saturating_i64_to_i32(value: i64) -> i32 {
    if value > i32::MAX as i64 {
        i32::MAX
    } else if value < i32::MIN as i64 {
        i32::MIN
    } else {
        value as i32
    }
}

const fn saturating_u32(value: u64) -> u32 {
    if value > u32::MAX as u64 {
        u32::MAX
    } else {
        value as u32
    }
}

/// Intersection of `viewport` and `rect`; `None` when they do not overlap.
fn intersect(viewport: RectPx, rect: RectPx) -> Option<RectPx> {
    let left = i64::from(rect.x).max(0);
    let top = i64::from(rect.y).max(0);
    let right = (i64::from(rect.x) + i64::from(rect.width)).min(i64::from(viewport.width));
    let bottom = (i64::from(rect.y) + i64::from(rect.height)).min(i64::from(viewport.height));
    if right <= left || bottom <= top {
        return None;
    }
    Some(RectPx::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    ))
}

/// Viewport pixel extent for a grid (saturating).
#[must_use]
pub fn viewport_extent(metrics: CellMetrics, cols: u16, rows: u16) -> ExtentPx {
    metrics.extent_for(usize::from(cols), usize::from(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: CellMetrics = CellMetrics {
        width: 8,
        height: 16,
    };

    fn tiny_red() -> (u32, u32, Vec<u8>) {
        // 2x2 opaque red.
        (2, 2, [0xFF, 0x00, 0x00, 0xFF].repeat(4))
    }

    fn stored_red(layer: &mut KittyImageLayer) -> KittyImageId {
        let (w, h, rgba) = tiny_red();
        layer.store(w, h, rgba, 67).unwrap()
    }

    #[test]
    fn action_mapping() {
        assert_eq!(KittyAction::from_a(None), KittyAction::TransmitAndDisplay);
        assert_eq!(
            KittyAction::from_a(Some('T')),
            KittyAction::TransmitAndDisplay
        );
        assert_eq!(KittyAction::from_a(Some('t')), KittyAction::Transmit);
        assert_eq!(
            KittyAction::from_a(Some('p')),
            KittyAction::Unsupported('p')
        );
        assert_eq!(
            KittyAction::from_a(Some('d')),
            KittyAction::Unsupported('d')
        );
        assert!(KittyAction::TransmitAndDisplay.displays());
        assert!(!KittyAction::Transmit.displays());
        assert!(!KittyAction::Unsupported('q').displays());
    }

    #[test]
    fn store_and_lookup() {
        let mut layer = KittyImageLayer::new();
        assert!(layer.is_empty());
        let id = stored_red(&mut layer);
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.total_bytes(), 16);
        let img = layer.get(id).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.rgba.len(), 16);
        assert_eq!(img.compressed_len, 67);
    }

    #[test]
    fn store_validates_before_admission() {
        let mut layer = KittyImageLayer::new();
        // Side cap fires on a 4-byte lie (no allocation of the refused size).
        assert_eq!(
            layer.store(100_000, 100_000, vec![0; 4], 4),
            Err(KittyPlacementError::DimensionsTooLarge {
                width: 100_000,
                height: 100_000,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        // Area cap: 5000x5000.
        assert_eq!(
            layer.store(5000, 5000, vec![0; 8], 8),
            Err(KittyPlacementError::TooManyPixels {
                pixels: 25_000_000,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
        // Length mismatch.
        assert_eq!(
            layer.store(2, 1, vec![0; 7], 7),
            Err(KittyPlacementError::LengthMismatch {
                expected: 8,
                actual: 7
            })
        );
        assert!(layer.is_empty());
        assert_eq!(layer.total_bytes(), 0);
    }

    #[test]
    fn transmit_only_stores_without_placing() {
        // The action model: Transmit admits bytes but no placement paints.
        let mut layer = KittyImageLayer::new();
        let action = KittyAction::from_a(Some('t'));
        assert!(!action.displays());
        let id = stored_red(&mut layer);
        assert!(layer.get(id).is_some());
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn unsupported_action_stores_without_painting() {
        let mut layer = KittyImageLayer::new();
        for a in ['p', 'd', 'q', 'f', 'x'] {
            let action = KittyAction::from_a(Some(a));
            assert!(!action.displays(), "a={a}");
        }
        let id = stored_red(&mut layer);
        assert!(layer.get(id).is_some());
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn display_uses_explicit_cell_span() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        // 2x2 px at 8x16 cells with explicit 3x2 cells.
        let pid = layer.display(id, 4, 5, 3, 2, METRICS, 0, 0).unwrap();
        let rect =
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0)
                .unwrap();
        assert_eq!(rect, RectPx::new(4 * 8, 5 * 16, 3 * 8, 2 * 16));
    }

    #[test]
    fn display_derives_span_from_pixels() {
        let mut layer = KittyImageLayer::new();
        // 20x40 px at 8x16 cells derives ceil(20/8)=3 x ceil(40/16)=3.
        let id = layer.store(20, 40, vec![7; 20 * 40 * 4], 100).unwrap();
        let pid = layer.display(id, 1, 2, 0, 0, METRICS, 0, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();
        assert_eq!((placement.cols, placement.rows), (3, 3));
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 0).unwrap();
        assert_eq!(rect, RectPx::new(8, 32, 24, 48));
    }

    #[test]
    fn display_unknown_image_fails_closed() {
        let mut layer = KittyImageLayer::new();
        assert_eq!(
            layer.display(KittyImageId(999), 0, 0, 1, 1, METRICS, 0, 0),
            Err(KittyPlacementError::ImageNotFound(KittyImageId(999)))
        );
        assert!(layer.placement_is_empty());
    }

    #[test]
    fn rect_clamped_to_viewport() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        // 10x10 cells at col 78 of an 80-col grid: clipped to 2 cols.
        let pid = layer.display(id, 78, 0, 10, 10, METRICS, 0, 0).unwrap();
        let rect =
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0)
                .unwrap();
        assert_eq!(rect, RectPx::new(78 * 8, 0, 2 * 8, 10 * 16));
    }

    #[test]
    fn full_rect_stays_unclamped_when_placement_overflows() {
        // #1334: a placement taller than the viewport keeps its full
        // extent; the clamped rect is the visible window into it.
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        // 2x4 cells at row 22 of a 24-row grid: 2 rows overflow.
        let pid = layer.display(id, 0, 22, 2, 4, METRICS, 0, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();
        let full = KittyImageLayer::placement_full_rect(placement, METRICS, 0).unwrap();
        assert_eq!(full, RectPx::new(0, 22 * 16, 2 * 8, 4 * 16));
        let visible = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 0).unwrap();
        assert_eq!(visible, RectPx::new(0, 22 * 16, 2 * 8, 2 * 16));
        // The id-resolving wrappers agree.
        assert_eq!(placement_full_rect_for(&layer, pid, METRICS, 0), Some(full));
        assert_eq!(
            placement_rect_for(&layer, pid, METRICS, 80, 24, 0),
            Some(visible)
        );
        assert_eq!(
            placement_full_rect_for(&layer, KittyPlacementId(999), METRICS, 0),
            None
        );
    }

    #[test]
    fn fully_outside_viewport_paints_nothing() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 90, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert_eq!(
            KittyImageLayer::placement_rect(layer.get_placement(pid).unwrap(), METRICS, 80, 24, 0),
            None
        );
        // Row past the bottom likewise.
        let pid2 = layer.display(id, 0, 30, 2, 2, METRICS, 0, 0).unwrap();
        assert_eq!(
            KittyImageLayer::placement_rect(layer.get_placement(pid2).unwrap(), METRICS, 80, 24, 0),
            None
        );
    }

    #[test]
    fn scroll_moves_anchor_with_content() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 0, 10, 2, 2, METRICS, 100, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();
        // No scroll: row 10.
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 100).unwrap();
        assert_eq!(rect.y, 10 * 16);
        // Three lines scrolled: row 7.
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 103).unwrap();
        assert_eq!(rect.y, 7 * 16);
        // Eleven lines scrolled: anchor row 10 moved to -1, but row 11 is at row 0 (1 row visible at top).
        let rect = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 111).unwrap();
        assert_eq!(rect.y, 0);
        assert_eq!(rect.height, 16);
        // Scrolled fully off the top (12 lines scrolled: 10 + 2 - 12 = 0): nothing paints.
        assert_eq!(
            KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 112),
            None
        );
    }

    #[test]
    fn alt_screen_clear_drops_everything() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        layer.display(id, 0, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert!(!layer.is_empty());
        assert!(!layer.placement_is_empty());
        layer.clear();
        assert!(layer.is_empty());
        assert!(layer.placement_is_empty());
        assert_eq!(layer.total_bytes(), 0);
    }

    #[test]
    fn remove_drops_image_and_its_placements() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let pid = layer.display(id, 0, 0, 2, 2, METRICS, 0, 0).unwrap();
        assert!(layer.remove(id));
        assert!(layer.get(id).is_none());
        assert!(layer.get_placement(pid).is_none());
        assert_eq!(layer.total_bytes(), 0);
        assert!(!layer.remove(id));
    }

    #[test]
    fn oversize_placement_fails_closed() {
        let mut layer = KittyImageLayer::new();
        // A bitmap that passes per-axis (8192x2048 = 16M px = area cap)
        // would still be admitted by a decoder; placement validates the same
        // Core-retained caps, so prove the area edge rejects here too
        // without allocating the refused 64 MiB: pass a short buffer and
        // expect LengthMismatch only after the caps pass — instead use
        // over-area dims directly.
        assert_eq!(
            layer.store(8192, 8192, vec![0; 4], 4),
            Err(KittyPlacementError::TooManyPixels {
                pixels: 67_108_864,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
        assert!(layer.is_empty());
    }

    #[test]
    fn paint_order_is_z_ascending_stable() {
        let mut layer = KittyImageLayer::new();
        let a = stored_red(&mut layer);
        let b = stored_red(&mut layer);
        let p1 = layer.display(a, 0, 0, 1, 1, METRICS, 0, 5).unwrap();
        let p2 = layer.display(b, 0, 0, 1, 1, METRICS, 0, -1).unwrap();
        let p3 = layer.display(a, 0, 0, 1, 1, METRICS, 0, 5).unwrap();
        let ordered = layer.placements_in_paint_order();
        let ids: Vec<KittyPlacementId> = ordered.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![p2, p1, p3]);
    }

    #[test]
    fn origin_binding_is_per_placement_and_filtered() {
        // CTX-0254: placements carry their emitting stream's origin; the
        // present layer paints only the focused leaf's origin.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        let primary = layer.display(img, 0, 0, 1, 1, METRICS, 0, 0).unwrap();
        let pane = layer
            .display_for_origin(img, 0, 0, 1, 1, METRICS, 0, 0, Some(7))
            .unwrap();
        assert_eq!(layer.get_placement(primary).unwrap().origin, None);
        assert_eq!(
            layer.get_placement(pane).unwrap().origin,
            Some(7),
            "pane placement must keep its origin token"
        );
        let for_primary: Vec<KittyPlacementId> = layer
            .placements_in_paint_order_for(None)
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(for_primary, vec![primary]);
        let for_pane: Vec<KittyPlacementId> = layer
            .placements_in_paint_order_for(Some(7))
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(for_pane, vec![pane]);
        assert!(
            layer.placements_in_paint_order_for(Some(9)).is_empty(),
            "unrelated origins paint nothing"
        );
        assert!(!layer.placement_for_origin_is_empty(None));
        assert!(!layer.placement_for_origin_is_empty(Some(7)));
        assert!(layer.placement_for_origin_is_empty(Some(9)));
    }

    #[test]
    fn clear_origin_keeps_other_origins_and_images() {
        // CTX-0254: one pane entering the alternate screen must not wipe
        // another pane's placements; stored images stay inert.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        layer.display(img, 0, 0, 1, 1, METRICS, 0, 0).unwrap();
        layer
            .display_for_origin(img, 0, 0, 1, 1, METRICS, 0, 0, Some(7))
            .unwrap();
        assert_eq!(layer.placement_len(), 2);
        layer.clear_origin(Some(7));
        assert_eq!(layer.placement_len(), 1);
        assert!(layer.placement_for_origin_is_empty(Some(7)));
        assert!(!layer.placement_for_origin_is_empty(None));
        assert_eq!(layer.len(), 1, "images survive origin clears, inert");
        layer.clear_origin(None);
        assert!(layer.placement_is_empty());
        assert_eq!(layer.len(), 1);
    }

    #[test]
    fn placement_cap_evicts_oldest() {
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let mut first = None;
        for i in 0..KITTY_PLACE_MAX_ITEMS + 5 {
            let pid = layer.display(id, 0, 0, 1, 1, METRICS, 0, i as i32).unwrap();
            if i == 0 {
                first = Some(pid);
            }
        }
        assert_eq!(layer.placement_len(), KITTY_PLACE_MAX_ITEMS);
        assert!(layer.get_placement(first.unwrap()).is_none());
    }

    #[test]
    fn caps_match_retained_policy_values() {
        // Compile-time: placement enforces exactly the decode ceilings.
        const _: () = assert!(KITTY_PLACE_MAX_BYTES == 256 * 1024 * 1024);
        const _: () = assert!(KITTY_PLACE_MAX_ITEMS == 128);
        assert_eq!(KITTY_PLACE_MAX_IMAGES, crate::kitty::KITTY_MAX_PLACEHOLDERS);
        // Error strings stay stable for snapshot greps.
        assert_eq!(
            KittyPlacementError::ImageNotFound(KittyImageId(7)).to_string(),
            "kitty placement image not found: 7"
        );
    }

    #[test]
    fn zero_size_store_rejected() {
        let mut layer = KittyImageLayer::new();
        assert_eq!(
            layer.store(0, 1, vec![0; 4], 4),
            Err(KittyPlacementError::LengthMismatch {
                expected: 0,
                actual: 4
            })
        );
        assert!(layer.is_empty());
    }

    #[test]
    fn placement_admits_wide_panorama_within_area_budget() {
        // F1 (CTX-0252, doc->code): the enforced side cap is 8192, not the
        // 4096 the old decode doc overclaimed. 8192x2048 is exactly the
        // 4096^2 area cap (64 MiB RGBA): the side/area/byte edge, admitted.
        let mut layer = KittyImageLayer::new();
        let id = layer
            .store(8192, 2048, vec![0x7F; 8192_usize * 2048 * 4], 1_000_000)
            .expect("wide panorama within area budget must be admitted");
        let img = layer.get(id).unwrap();
        assert_eq!((img.width, img.height), (8192, 2048));
        assert_eq!(layer.total_bytes(), 64 * 1024 * 1024);
    }

    #[test]
    fn side_cap_matches_retained_ceiling() {
        // Placement re-enforces exactly the Core-retained ceilings
        // (8192/side, mirrors the versioned graphics contract).
        let mut layer = KittyImageLayer::new();
        assert_eq!(
            layer.store(8193, 1, vec![0; 4], 4),
            Err(KittyPlacementError::DimensionsTooLarge {
                width: 8193,
                height: 1,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        assert_eq!(KITTY_DECODE_MAX_DIMENSION, 8192);
        assert!(layer.is_empty());
    }

    #[test]
    fn compressed_len_is_diagnostic_only() {
        // F1: the wire payload length never gates admission — a 10 MiB claim
        // (2.5x the generic IMG-1 4 MiB cap) stores exactly like 0. Memory
        // is bounded by decoded output caps, not wire length.
        let mut layer = KittyImageLayer::new();
        let (w, h, rgba) = tiny_red();
        let big = layer
            .store(w, h, rgba.clone(), 10 * 1024 * 1024)
            .expect("large compressed_len must not refuse admission");
        let zero = layer
            .store(w, h, rgba, 0)
            .expect("zero compressed_len must not refuse admission");
        assert_eq!(layer.get(big).unwrap().compressed_len, 10 * 1024 * 1024);
        assert_eq!(layer.get(zero).unwrap().compressed_len, 0);
    }

    #[test]
    fn scroll_clips_top_edge_instead_of_premature_drop() {
        let mut layer = KittyImageLayer::new();
        // 4x4 px stored image; the test pins scroll-adjusted rect policy
        // (scaling itself lives in the extension).
        let image_rgba = vec![0xCC; 4 * 4 * 4];
        let image_id = layer.store(4, 4, image_rgba, 64).unwrap();
        // Place image: anchor_col 0, anchor_row 5, 2 cols x 10 rows.
        // Base scrollback = 0.
        let pid = layer.display(image_id, 0, 5, 2, 10, METRICS, 0, 0).unwrap();
        let placement = layer.get_placement(pid).unwrap();

        // 1. Unscrolled (scrollback = 0):
        let full = KittyImageLayer::placement_full_rect(placement, METRICS, 0).unwrap();
        assert_eq!(full, RectPx::new(0, 5 * 16, 2 * 8, 10 * 16));
        let visible = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 0).unwrap();
        assert_eq!(visible, RectPx::new(0, 5 * 16, 2 * 8, 10 * 16));

        // 2. Scrolled by 8 lines (scrollback = 8):
        // anchor_row (5) - scrolled (8) = -3 rows.
        // bottom_row is 5 + 10 - 8 = 7 rows > 0 (still visible in viewport!).
        // Previously checked_sub(8) failed on anchor_row (5) and returned None.
        let full_scrolled = KittyImageLayer::placement_full_rect(placement, METRICS, 8)
            .expect("must not prematurely drop when top edge is clipped");
        assert_eq!(full_scrolled, RectPx::new(0, -3 * 16, 2 * 8, 10 * 16));
        let visible_scrolled = KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 8)
            .expect("visible portion must be present");
        assert_eq!(visible_scrolled, RectPx::new(0, 0, 2 * 8, 7 * 16));

        // 3. Scrolled by 15 lines (scrollback = 15):
        // 5 + 10 - 15 = 0 rows <= 0 (completely off the top).
        assert_eq!(
            KittyImageLayer::placement_full_rect(placement, METRICS, 15),
            None
        );
        assert_eq!(
            KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 15),
            None
        );
    }

    #[test]
    fn precheck_empty_rejected_first() {
        // Empty carries no image, for every channel shape (mirrors the moved
        // decoder ordering: emptiness fires before dimension checks).
        assert_eq!(
            precheck_declared_image(None, None, None, 0),
            Err(KittyPrecheckError::EmptyPayload)
        );
        assert_eq!(
            precheck_declared_image(Some(4), Some(2), Some(2), 0),
            Err(KittyPrecheckError::EmptyPayload)
        );
    }

    #[test]
    fn precheck_raw_missing_dimensions() {
        assert_eq!(
            precheck_declared_image(Some(3), None, None, 12),
            Err(KittyPrecheckError::MissingDimensions)
        );
        assert_eq!(
            precheck_declared_image(Some(3), Some(2), None, 12),
            Err(KittyPrecheckError::MissingDimensions)
        );
        assert_eq!(
            precheck_declared_image(Some(4), None, Some(2), 12),
            Err(KittyPrecheckError::MissingDimensions)
        );
    }

    #[test]
    fn precheck_zero_dimension_rejected() {
        for (w, h) in [(0, 1), (1, 0), (0, 0)] {
            assert_eq!(
                precheck_declared_image(Some(4), Some(w), Some(h), 4),
                Err(KittyPrecheckError::ZeroDimension),
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn precheck_side_cap_before_alloc() {
        // 100_000 x 100_000 would need tens of GB; the side cap fires on a
        // 4-byte payload, proving bounds run before allocation.
        assert_eq!(
            precheck_declared_image(Some(3), Some(100_000), Some(100_000), 4),
            Err(KittyPrecheckError::DimensionsTooLarge {
                width: 100_000,
                height: 100_000,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
        assert_eq!(
            precheck_declared_image(Some(4), Some(KITTY_DECODE_MAX_DIMENSION + 1), Some(1), 4),
            Err(KittyPrecheckError::DimensionsTooLarge {
                width: KITTY_DECODE_MAX_DIMENSION + 1,
                height: 1,
                cap: KITTY_DECODE_MAX_DIMENSION,
            })
        );
    }

    #[test]
    fn precheck_area_cap_before_alloc() {
        // 5000x5000 = 25M px > 16.7M cap; would-be 100 MB RGBA never allocs.
        assert_eq!(
            precheck_declared_image(Some(4), Some(5000), Some(5000), 8),
            Err(KittyPrecheckError::TooManyPixels {
                pixels: 25_000_000,
                cap: KITTY_DECODE_MAX_PIXELS,
            })
        );
    }

    #[test]
    fn precheck_raw_length_must_match_exactly() {
        // 2x1 RGB needs exactly 6 bytes; 2x1 RGBA exactly 8.
        assert_eq!(
            precheck_declared_image(Some(3), Some(2), Some(1), 5),
            Err(KittyPrecheckError::LengthMismatch {
                expected: 6,
                actual: 5
            })
        );
        assert_eq!(
            precheck_declared_image(Some(3), Some(2), Some(1), 7),
            Err(KittyPrecheckError::LengthMismatch {
                expected: 6,
                actual: 7
            })
        );
        assert!(precheck_declared_image(Some(3), Some(2), Some(1), 6).is_ok());
        assert!(precheck_declared_image(Some(4), Some(2), Some(1), 8).is_ok());
    }

    #[test]
    fn precheck_png_ignores_declared_dimensions() {
        // Declared `s`/`v` are meaningless for PNG: the Core-owned decoder
        // validates the IHDR before allocating, so any declaration passes
        // the pre-check (emptiness aside).
        assert!(precheck_declared_image(None, Some(99), Some(99), 64).is_ok());
        assert!(precheck_declared_image(None, None, None, 64).is_ok());
    }

    #[test]
    fn precheck_error_display_stable() {
        assert_eq!(
            KittyPrecheckError::EmptyPayload.to_string(),
            "kitty payload is empty"
        );
        assert_eq!(
            KittyPrecheckError::MissingDimensions.to_string(),
            "kitty raw payload needs width and height (s/v)"
        );
        assert_eq!(
            KittyPrecheckError::ZeroDimension.to_string(),
            "kitty image dimension is zero"
        );
        assert_eq!(
            KittyPrecheckError::DimensionsTooLarge {
                width: 9000,
                height: 1,
                cap: 8192
            }
            .to_string(),
            "kitty image 9000x1 exceeds max dimension of 8192px"
        );
        assert_eq!(
            KittyPrecheckError::TooManyPixels {
                pixels: 25_000_000,
                cap: 16_777_216
            }
            .to_string(),
            "kitty image of 25000000 pixels exceeds max of 16777216 pixels"
        );
        assert_eq!(
            KittyPrecheckError::DecodedTooLarge {
                bytes: 100,
                cap: 64
            }
            .to_string(),
            "kitty decoded bitmap of 100 bytes exceeds max of 64 bytes"
        );
        assert_eq!(
            KittyPrecheckError::LengthMismatch {
                expected: 6,
                actual: 5
            }
            .to_string(),
            "kitty raw payload of 5 bytes does not match 6 expected bytes"
        );
        assert!(
            KittyPrecheckError::DecoderUnavailable
                .to_string()
                .starts_with("kitty decoder unavailable")
        );
        assert_eq!(
            KittyPrecheckError::MalformedPng("truncated".to_owned()).to_string(),
            "kitty PNG is malformed: truncated"
        );
    }

    #[test]
    fn decode_error_maps_into_precheck_taxonomy() {
        use crate::kitty_decode::KittyDecodeError as D;
        assert_eq!(
            KittyPrecheckError::from(D::EmptyPayload),
            KittyPrecheckError::EmptyPayload
        );
        assert_eq!(
            KittyPrecheckError::from(D::LengthMismatch {
                expected: 6,
                actual: 5
            }),
            KittyPrecheckError::LengthMismatch {
                expected: 6,
                actual: 5
            }
        );
        assert_eq!(
            KittyPrecheckError::from(D::MalformedPng("x".to_owned())),
            KittyPrecheckError::MalformedPng("x".to_owned())
        );
    }

    #[test]
    fn precheck_caps_hold_ledger_relationship() {
        // Core-owned re-assertion of the moved decoder's ledger bound
        // (threat T-01/T-02): no admissible bitmap rivals stored+in-flight
        // pressure.
        const _: () = assert!(KITTY_DECODE_MAX_BYTES * 4 < crate::kitty::KITTY_LEDGER_MAX_BYTES);
        const _: () = assert!(KITTY_DECODE_MAX_PIXELS == 4096 * 4096);
        const _: () = assert!(KITTY_DECODE_MAX_PIXELS * 4 == KITTY_DECODE_MAX_BYTES as u64);
        assert_eq!(KITTY_DECODE_MAX_DIMENSION, 8192);
        assert_eq!(KITTY_DECODE_MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(KITTY_FORMAT_PNG, 100);
        assert_eq!(KITTY_FORMAT_RGB, 24);
        assert_eq!(KITTY_FORMAT_RGBA, 32);
        assert_eq!(KITTY_PRESENT_MAX_BLITS_PER_FRAME, 32);
        assert_eq!(KITTY_PRESENT_MAX_BYTES_PER_FRAME, 64 * 1024 * 1024);
        assert_eq!(KITTY_CURSOR_MAX_SCROLL_LINES_PER_PLACEMENT, 256);
    }

    #[test]
    fn delete_by_wire_is_origin_scoped_and_pinned() {
        // CTX-1072 (#1850): protocol-level deletion clears rendered
        // placements through the origin-scoped wire identity mapping.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        layer
            .display_for_origin_with_wire(img, 0, 0, 1, 1, METRICS, 0, 0, None, 7, 0)
            .unwrap();
        layer
            .display_for_origin_with_wire(img, 0, 0, 1, 1, METRICS, 0, 0, Some(7), 7, 0)
            .unwrap();
        layer
            .display_for_origin_with_wire(img, 0, 0, 1, 1, METRICS, 0, 0, None, 7, 1)
            .unwrap();
        assert_eq!(layer.placement_len(), 3);
        // Pinned delete removes only the pinned placement on that origin.
        assert_eq!(layer.delete_by_wire(None, 7, Some(1)), 1);
        assert_eq!(layer.placement_len(), 2);
        // Anonymous placements never match a pin.
        assert_eq!(layer.delete_by_wire(None, 7, Some(9)), 0);
        assert_eq!(layer.delete_by_wire(None, 7, Some(0)), 0);
        // Unpinned delete removes every placement of the image on that
        // origin only.
        assert_eq!(layer.delete_by_wire(None, 7, None), 1);
        assert_eq!(layer.placement_len(), 1);
        assert!(!layer.placement_for_origin_is_empty(Some(7)));
        assert_eq!(layer.delete_by_wire(Some(7), 7, None), 1);
        assert!(layer.placement_is_empty());
        // Stored images stay inert under the store caps.
        assert_eq!(layer.len(), 1);
    }

    #[test]
    fn maximal_row_span_stays_placeable_and_viewport_clamped() {
        // CTX-1072 (#1850): a maximal-row span is still admitted as a
        // placement; only the cursor-advance scroll is bounded downstream.
        // The visible rect stays viewport-clamped so raster work is bounded
        // by the viewport, never by the 65535-row span.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        let pid = layer
            .display_for_origin_with_wire(img, 0, 0, 1, u16::MAX, METRICS, 0, 0, None, 7, 0)
            .expect("maximal-row span must still place");
        let placement = layer.get_placement(pid).unwrap();
        assert_eq!(placement.rows, u16::MAX);
        assert_eq!((placement.wire_image, placement.wire_placement), (7, 0));
        let visible =
            KittyImageLayer::placement_rect(placement, METRICS, 80, 24, 0).expect("visible");
        assert_eq!(visible, RectPx::new(0, 0, 8, 24 * 16));
    }
}
