//! `bitty-rich`: rich presentation (OQ-008 image store, OQ-015 scene).
//!
//! Implements the accepted contracts from
//! `bitty-docs/docs/specifications/rich-presentation-rfc.md` (accepted
//! 2026-08-28, closes OQ-008, OQ-015, OQ-016 at design level) at the
//! headless, bounded layer. No GPU, no window system, no filesystem, no
//! unsafe.
//!
//! # Accepted contracts implemented
//!
//! - **OQ-008 ImageStore** (`image`): 256 MiB store / 64 MiB per-image
//!   decoded caps, 4 MiB compressed cap, 4096 x 4096 dimension cap,
//!   256 image count cap, 64 animation frames, 128 placements, FIFO
//!   eviction, overflow-checked allocation, alternate-screen suppression.
//! - **OQ-015 Scene** (`scene`): versioned `RichBlock` (v1), `SceneNode`
//!   declarative layout, `Scene` composition with SCN-1..5 limits (2048
//!   nodes/block, 32 depth, 256 KiB text/block, 2 MiB aggregated/terminal,
//!   64 blocks/terminal), deterministic `BlockId`/`ImageStore` attribution.
//!
//! `bitty_term_state::ImageStore` (64 entries, 4096 bytes each) remains as
//! the legacy terminal-truth placeholder seam; this crate's `image`
//! module is the RFC-compliant presentation store consumed by
//! `bitty-render`. The `kitty` stub is retained for compatibility and
//! mirrors the legacy term-state bounds; new code should use `image`.
//!
//! # Bounds (threat T-01/T-02)
//!
//! Every collection is bounded and deterministic (RFC `IMG-*` / `SCN-*`):
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`hyperlink::HYPERLINK_TABLE_MAX`] (via term-state) | 1024 | new distinct link degrades to no link |
//! | [`shell::SHELL_ZONE_MAX`] mirrors `ZONE_RECORDS_MAX` | 1024 | oldest dropped |
//! | [`clipboard::CLIPBOARD_MAX_HISTORY`] | 16 | oldest dropped |
//! | [`clipboard::CLIPBOARD_MAX_PAYLOAD_BYTES`] | 4096 | truncation at cap |
//! | [`kitty::KITTY_MAX_PLACEHOLDERS`] (legacy) | 64 | oldest evicted |
//! | [`kitty::KITTY_MAX_PAYLOAD_BYTES`] (legacy) | 4096 | truncation |
//! | [`image::IMAGE_STORE_MAX_COUNT`] (IMG-5) | 256 | oldest evicted on admission |
//! | [`image::IMAGE_STORE_MAX_BYTES`] (IMG-4) | 256 MiB | oldest evicted on admission |
//! | [`image::IMAGE_MAX_DECODED_BYTES`] (IMG-3) | 64 MiB | typed error, no placement |
//! | [`image::IMAGE_MAX_COMPRESSED_BYTES`] (IMG-1) | 4 MiB | typed error, no placement |
//! | [`image::IMAGE_MAX_DIMENSION`] (IMG-2) | 4096 | typed error |
//! | [`image::IMAGE_MAX_FRAMES`] (IMG-6) | 64 | excess discarded |
//! | [`image::IMAGE_MAX_PLACEMENTS`] (IMG-8) | 128 | oldest evicted |
//! | [`scene::SCENE_MAX_NODES_PER_BLOCK`] (SCN-1) | 2048 | typed error, retain last good |
//! | [`scene::SCENE_MAX_DEPTH`] (SCN-2) | 32 | typed error |
//! | [`scene::SCENE_MAX_TEXT_BYTES_PER_BLOCK`] (SCN-3) | 256 KiB | typed error |
//! | [`scene::SCENE_MAX_RICH_BYTES_PER_TERMINAL`] (SCN-4) | 2 MiB | typed error |
//! | [`scene::SCENE_MAX_BLOCKS_PER_TERMINAL`] (SCN-5) | 64 | typed error |
//!
//! # Headless seam
//!
//! No window system, no adapter, no clipboard I/O, and no image decoding
//! are performed here. All tests run on GPU-less CI via pure logic on
//! `State`/`Snapshot` values. Where rendering geometry is needed (hyperlink
//! underline rects, kitty placeholder rects) the caller supplies a
//! [`CellMetrics`] (`width x height` in pixels) and receives owned
//! [`RectPx`] values; no renderer is borrowed.

#![forbid(unsafe_code)]

pub mod clipboard;
pub mod geometry;
pub mod hyperlink;
pub mod image;
pub mod kitty;
pub mod loader;
pub mod presentation;
pub mod scene;
pub mod shell;

pub use clipboard::{ClipboardPolicy, ClipboardRequest, ClipboardState};
pub use geometry::{CellMetrics, ExtentPx, RectPx};
pub use hyperlink::{HyperlinkInfo, HyperlinkSpan};
pub use image::{
    AlternateScope, ClipRect, DecodedImage, ImageId, ImagePlacement, ImageSource, ImageStore,
    ImageStoreError, PixelFormat, PlacementAnchor, PlacementGeometry, PlacementId,
    ScrollBehavior as ImageScrollBehavior,
};
pub use kitty::{KittyGraphicsStub, KittyPlaceholder, KittyPlaceholderId};
pub use scene::{
    BlockAnchor, BlockId, Border, CodeBlockModel, ListModel, RichBlock,
    SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH, SCENE_MAX_NODES_PER_BLOCK,
    SCENE_MAX_RICH_BYTES_PER_TERMINAL, SCENE_MAX_TEXT_BYTES_PER_BLOCK, Scene, SceneError,
    SceneNode, ScrollBehavior, StyledSpan, TableModel,
};
pub use shell::{CommandRegion, ShellIntegration};
