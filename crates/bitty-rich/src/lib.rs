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
//! The `composer` module is the only filesystem/process seam in this crate:
//! the external-editor round-trip writes a `0600` temp file under the OS
//! temp dir and spawns `$VISUAL`/`$EDITOR` with a bounded timeout plus
//! kill. Everything else here is pure logic over `State`/`Snapshot` values.
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
//! | [`clipboard::CLIPBOARD_MAX_OUTSTANDING_GRANTS`] | 16 | oldest grant evicted (token dies) |
//! | [`kitty::KITTY_MAX_PLACEHOLDERS`] (legacy) | 64 | oldest evicted |
//! | [`kitty::KITTY_MAX_PAYLOAD_BYTES`] (legacy) | 4096 | truncation |
//! | [`kitty_decode::KITTY_DECODE_MAX_DIMENSION`] | 8192 px/side | typed error, no allocation |
//! | [`kitty_decode::KITTY_DECODE_MAX_PIXELS`] (4096² area) | 16.7M px | typed error, no allocation |
//! | [`kitty_decode::KITTY_DECODE_MAX_BYTES`] decoded RGBA | 64 MiB | typed error, no allocation |
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
//! | [`blocks::COMMAND_BLOCK_MAX`] command blocks | 256 | newest retained, oldest dropped |
//! | [`blocks::FOLD_MAX`] folded ids | 256 | fold fails closed at cap |
//! | [`hints::HINT_TARGET_MAX`] hint targets / labels | 256 | register fails closed; batch sheds sorted tail |
//! | hint label text per batch | [`hints::HINT_TEXT_MAX_BYTES`] (8 KiB) | allocation stops at budget, remainder shed |
//! | hint batches as overlays | `0` slots | single annotation layer bypasses (never consumes) the `4+1` bound |
//! | [`composer::COMPOSER_MAX_BYTES`] composer buffer / temp file | 64 KiB | insert/frame/edit fail closed, buffer kept, temp deleted |
//! | composer editor wait | [`composer::EDITOR_TIMEOUT_MAX`] (300 s) | kill + `Timeout` error, buffer kept, temp deleted |
//!
//! # Headless seam
//!
//! No window system, no adapter, no clipboard I/O, and no GPU presentation
//! are performed here. The only decoding is the bounded Kitty PNG/RGB/RGBA
//! to RGBA8 step in [`kitty_decode`] (fail-closed, allocation-checked, no
//! renderer coupling). All tests run on GPU-less CI via pure logic on
//! `State`/`Snapshot` values, except the composer external-editor round-trip
//! (OS temp file plus `$VISUAL`/`$EDITOR` child process, exercised with
//! fake editor scripts). Where rendering geometry is needed (hyperlink
//! underline rects, kitty placeholder rects) the caller supplies a
//! [`CellMetrics`] (`width x height` in pixels) and receives owned
//! [`RectPx`] values; no renderer is borrowed.

#![forbid(unsafe_code)]

pub mod blocks;
pub mod clipboard;
pub mod composer;
pub mod geometry;
pub mod hints;
pub mod hyperlink;
pub mod image;
pub mod kitty;
pub mod kitty_decode;
pub mod kitty_place;
pub mod loader;
pub mod presentation;
pub mod scene;
pub mod shell;

pub use blocks::{
    COMMAND_BLOCK_MAX, CommandBlock, CommandId, CommandState, FOLD_MAX, FoldState, SemanticRange,
    block_by_id, block_count, blocks, hidden_blocks, is_output_kind, list_blocks, visible_blocks,
};
pub use clipboard::{
    ClipboardGrantScope, ClipboardPolicy, ClipboardReadToken, ClipboardRequest, ClipboardState,
};
pub use composer::{
    BufferError, COMPOSER_MAX_BYTES, COMPOSER_OPEN_CHORD, ChordParseError, CommandBuffer,
    ComposerChord, ComposerFeedError, ComposerFeedOutcome, ComposerKey, ComposerKeyEvent,
    ComposerKeys, ComposerKeysError, ComposerSession, EDITOR_TIMEOUT_DEFAULT, EDITOR_TIMEOUT_MAX,
    EditorError, OpenChord, OpenChordError, PASTE_CLOSE, PASTE_OPEN, SUBMIT_TERMINATOR,
    TempComposerFile, edit_externally, frame_submit, normal_mode_passthrough, read_composer_back,
    resolve_editor, run_editor, should_auto_offer, validate_open_chord, write_composer_temp,
};
pub use geometry::{CellMetrics, ExtentPx, RectPx};
pub use hints::{
    ChordError, DispatchError, DispatchOutcome, HINT_LABEL_ALPHABET, HINT_LABEL_MAX_CHARS,
    HINT_OPERATOR_KEYS, HINT_TARGET_MAX, HINT_TEXT_MAX_BYTES, HintAction, HintActions, HintAnchor,
    HintBatch, HintChord, HintFeedError, HintKind, HintLabel, HintOperator, HintRegistry,
    HintScope, HintSession, HintTarget, OperatorConflict, TargetId, allocate_labels,
    check_operator_conflicts, collect_command_targets, collect_panel_targets, collect_view_targets,
    dispatch, label_for_index, parse_hint_chord,
};
pub use hyperlink::{HyperlinkInfo, HyperlinkSpan};
pub use image::{
    AlternateScope, ClipRect, DecodedImage, ImageId, ImagePlacement, ImageSource, ImageStore,
    ImageStoreError, PixelFormat, PlacementAnchor, PlacementGeometry, PlacementId,
    ScrollBehavior as ImageScrollBehavior,
};
pub use kitty::{KittyGraphicsStub, KittyPlaceholder, KittyPlaceholderId};
pub use kitty_decode::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS, KITTY_FORMAT_PNG,
    KITTY_FORMAT_RGB, KITTY_FORMAT_RGBA, KittyDecodeError, KittyDecodedImage, KittyTransmitFormat,
    decode_kitty_payload,
};
pub use kitty_place::{
    KITTY_PLACE_MAX_BYTES, KITTY_PLACE_MAX_IMAGES, KITTY_PLACE_MAX_ITEMS, KittyAction,
    KittyImageId, KittyImageLayer, KittyPlacedImage, KittyPlacement, KittyPlacementError,
    KittyPlacementId, placement_rect_for, rasterize, viewport_extent,
};
pub use scene::{
    BlockAnchor, BlockId, Border, CodeBlockModel, ListModel, RichBlock,
    SCENE_MAX_BLOCKS_PER_TERMINAL, SCENE_MAX_DEPTH, SCENE_MAX_NODES_PER_BLOCK,
    SCENE_MAX_RICH_BYTES_PER_TERMINAL, SCENE_MAX_TEXT_BYTES_PER_BLOCK, Scene, SceneError,
    SceneNode, ScrollBehavior, StyledSpan, TableModel,
};
pub use shell::{CommandRegion, ShellIntegration};
