//! Core-owned Kitty graphics seam (CTX-1186, issue #2000).
//!
//! This module freezes the decode/raster surface Core consumes, following
//! the `storage_backends.rs` pattern in one direction only: Core defines
//! the traits, and the `bitty-graphics` extension crate implements them on
//! its side. The small-core graph fence (W-147) keeps every extension
//! identifier out of Core sources and every production edge off the
//! extension crates, so no composition-root adapter naming the extension
//! can land in this repository — the in-repo implementation below is the
//! Core-local backend, and the extension binds to these same signatures,
//! caps, and error shapes on its side
//! (`bitty-graphics#15` seam contract).
//!
//! What is frozen here (mirrors the extension conformance suite):
//!
//! - [`KittyTransmitFormat`]: the wire-`f=` mapping with the seam
//!   spelling — unknown values reject *before* the call
//!   ([`KittyTransmitFormat::from_f`] returns `None`; the legacy wire-`u32`
//!   entry points in [`crate::kitty_decode`] keep their own `MalformedPng`
//!   spelling for direct callers).
//! - [`KittyDecodeBackend`] / [`KittyRasterBackend`]: decode (borrowed and
//!   owned) plus raster (whole-rect and clipped) over Core-owned carriers.
//! - [`KittyBackend`] / [`KittyBackendHandle`]: the dyn-safe composition of
//!   both traits that the composition root holds and injects, so runtime
//!   call sites dispatch through one object and never name the free
//!   functions ([`CoreKittyBackend`] is the in-tree binding).
//! - Caps, verbatim: 8192 px/side, 4096 x 4096 px area, 64 MiB RGBA,
//!   32 blits and 64 MiB per frame (compile-time asserted below against
//!   the [`crate::kitty_place`] twins so drift fails the build, not a
//!   test run).
//!
//! Explicit binding deltas (see the extension binding map):
//!
//! - Raster-cache entry cap: Core keeps 32 (one per per-frame blit slot);
//!   the extension sizes its own cache at 128. The 64 MiB byte ceiling is
//!   the shared invariant.
//! - The extension's additive surface (`KittyFrameBudget`,
//!   `get_or_rasterize`, animation-frame carriers) has no Core
//!   counterpart and is not bound here.

use super::kitty_decode::{
    KittyDecodeError, KittyDecodedImage, decode_kitty_payload, decode_kitty_payload_owned,
};
use super::kitty_place::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS, KITTY_FORMAT_PNG,
    KITTY_FORMAT_RGB, KITTY_FORMAT_RGBA, KITTY_PRESENT_MAX_BLITS_PER_FRAME,
    KITTY_PRESENT_MAX_BYTES_PER_FRAME, KittyPlacedImage,
};
use super::kitty_raster::{rasterize_kitty, rasterize_kitty_clipped, rasterize_rgba_clipped};
use crate::geometry::RectPx;

// Seam-frozen numbers: drift in the `kitty_place` twins fails the build.
const _: () = assert!(KITTY_DECODE_MAX_DIMENSION == 8192);
const _: () = assert!(KITTY_DECODE_MAX_PIXELS == 4096 * 4096);
const _: () = assert!(KITTY_DECODE_MAX_BYTES == 64 * 1024 * 1024);
const _: () = assert!(KITTY_PRESENT_MAX_BLITS_PER_FRAME == 32);
const _: () = assert!(KITTY_PRESENT_MAX_BYTES_PER_FRAME == 64 * 1024 * 1024);
const _: () = assert!(KITTY_FORMAT_PNG == 100);
const _: () = assert!(KITTY_FORMAT_RGB == 24);
const _: () = assert!(KITTY_FORMAT_RGBA == 32);

/// Kitty wire `f=` format with the seam spelling.
///
/// Only the formats terminals actually transmit are represented; anything
/// else rejects at [`Self::from_f`] instead of being guessed. Unknown wire
/// values never reach a [`KittyDecodeBackend`] entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyTransmitFormat {
    /// `f=100`: PNG data, dimensions read from `IHDR`.
    Png,
    /// `f=24`: raw RGB, 3 bytes per pixel, dimensions from `s`/`v`.
    Rgb,
    /// `f=32`: raw RGBA, 4 bytes per pixel, dimensions from `s`/`v`.
    Rgba,
}

impl KittyTransmitFormat {
    /// Maps a wire `f=` value to a supported format.
    ///
    /// Returns `None` for every value outside `{100, 24, 32}`; callers
    /// reject rather than guess.
    #[must_use]
    pub const fn from_f(value: u32) -> Option<Self> {
        match value {
            KITTY_FORMAT_PNG => Some(Self::Png),
            KITTY_FORMAT_RGB => Some(Self::Rgb),
            KITTY_FORMAT_RGBA => Some(Self::Rgba),
            _ => None,
        }
    }

    /// Wire `f=` value for this format.
    #[must_use]
    pub const fn f_value(self) -> u32 {
        match self {
            Self::Png => KITTY_FORMAT_PNG,
            Self::Rgb => KITTY_FORMAT_RGB,
            Self::Rgba => KITTY_FORMAT_RGBA,
        }
    }

    /// Bytes per pixel for raw formats; `None` for PNG (`IHDR` governs).
    #[must_use]
    pub const fn raw_channels(self) -> Option<usize> {
        match self {
            Self::Png => None,
            Self::Rgb => Some(3),
            Self::Rgba => Some(4),
        }
    }
}

/// Core-owned Kitty decode backend: bounded payload decode behind the seam.
///
/// Bounds run before allocation on every path; every rejection returns
/// [`KittyDecodeError`] and no partial bitmap is ever surfaced as `Ok`.
/// Decoding is a pure function of `(format, dimensions, payload)`.
pub trait KittyDecodeBackend {
    /// Decodes an assembled Kitty payload into an RGBA8 bitmap.
    ///
    /// PNG ignores supplied dimensions (`IHDR` governs); raw formats
    /// require both. See [`decode_kitty_payload`] for the error taxonomy.
    fn decode_transmit(
        &self,
        format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        payload: &[u8],
    ) -> Result<KittyDecodedImage, KittyDecodeError>;

    /// Decodes from an owned buffer (zero-copy for `f=32`, in-place
    /// expansion for `f=24`; PNG still decodes through the codec borrow).
    /// Output is byte-identical to [`Self::decode_transmit`].
    fn decode_transmit_owned(
        &self,
        format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        payload: Box<[u8]>,
    ) -> Result<KittyDecodedImage, KittyDecodeError>;
}

/// Core-owned Kitty raster backend: bounded scaling behind the seam.
///
/// Nearest-neighbor scaling to placement rects; `None` paints nothing
/// (empty rects, failed validation — always before allocation). Output is
/// straight-alpha RGBA8, row-major.
pub trait KittyRasterBackend {
    /// Scales a stored bitmap to the exact `rect` extent.
    fn rasterize_placed(&self, image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>>;

    /// Scales into `full`, emitting only the `visible` window (`visible`
    /// must lie inside `full`). Bit-identical to scaling into `full` and
    /// cropping `visible`, but only the visible bytes are allocated.
    fn rasterize_placed_clipped(
        &self,
        image: &KittyPlacedImage,
        full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>>;

    /// Scales one raw RGBA8 frame triple to the visible window: identical
    /// to [`Self::rasterize_placed_clipped`] except the source is an
    /// explicit `(width, height, rgba)` triple instead of a stored image,
    /// so animation frames rasterize through the same validated path
    /// (same caps, same scale-then-crop semantics).
    fn rasterize_frame(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>>;
}

/// Complete Core-owned Kitty graphics backend: decode and raster behind one
/// dyn-safe object.
///
/// The two mechanics traits stay separate so each can be bound and tested on
/// its own; this supertrait is what the composition root actually holds, so a
/// runtime stores one [`KittyBackendHandle`] instead of two seam objects. It
/// is `Send + Sync + Debug` so the handle fits any runtime threading shape and
/// keeps the owning type's `Debug` impl derivable.
///
/// Every implementation of [`KittyDecodeBackend`] + [`KittyRasterBackend`] is
/// a [`KittyBackend`] through the blanket impl; the in-tree implementation is
/// [`CoreKittyBackend`].
pub trait KittyBackend:
    KittyDecodeBackend + KittyRasterBackend + Send + Sync + std::fmt::Debug
{
}

impl<T> KittyBackend for T where
    T: KittyDecodeBackend + KittyRasterBackend + Send + Sync + std::fmt::Debug
{
}

/// Shared dyn handle to a [`KittyBackend`] — the composition-root shape.
///
/// `Arc` (not `Box`) so the injecting caller can keep its own handle (counters
/// in tests, a shared backend in the app) while the runtime owns the same
/// object. Construction defaults to `Arc::new(CoreKittyBackend::new())`; the
/// composition root replaces it through the runtime setter
/// (`Runtime::set_kitty_backend`).
pub type KittyBackendHandle = std::sync::Arc<dyn KittyBackend>;

/// In-repo [`KittyDecodeBackend`] + [`KittyRasterBackend`]: the Core-owned
/// decode/raster mechanics behind the seam traits.
///
/// Stateless (every bound comes from Core constants and the shared minimal
/// codec); `Copy` so the composition root can build the default handle inline
/// where the transmit and present paths need it. The extension implements the
/// same traits on its side; swapping implementations never touches the call
/// sites, which name only the traits and this backend type — never an
/// extension crate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CoreKittyBackend;

impl CoreKittyBackend {
    /// Builds the backend (stateless: no configuration, no handles).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl KittyDecodeBackend for CoreKittyBackend {
    fn decode_transmit(
        &self,
        format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        payload: &[u8],
    ) -> Result<KittyDecodedImage, KittyDecodeError> {
        decode_kitty_payload(format.f_value(), width, height, payload)
    }

    fn decode_transmit_owned(
        &self,
        format: KittyTransmitFormat,
        width: Option<u32>,
        height: Option<u32>,
        payload: Box<[u8]>,
    ) -> Result<KittyDecodedImage, KittyDecodeError> {
        decode_kitty_payload_owned(format.f_value(), width, height, payload)
    }
}

impl KittyRasterBackend for CoreKittyBackend {
    fn rasterize_placed(&self, image: &KittyPlacedImage, rect: RectPx) -> Option<Vec<u8>> {
        rasterize_kitty(image, rect)
    }

    fn rasterize_placed_clipped(
        &self,
        image: &KittyPlacedImage,
        full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>> {
        rasterize_kitty_clipped(image, full, visible)
    }

    fn rasterize_frame(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        full: RectPx,
        visible: RectPx,
    ) -> Option<Vec<u8>> {
        rasterize_rgba_clipped(width, height, rgba, full, visible)
    }
}
