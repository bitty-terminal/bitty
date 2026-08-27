//! `bitty-rich`: rich presentation draft (images, hyperlinks, shell integration).
//!
//! This crate implements the "rich presentation draft" phase of the
//! build-order spine and the OQ-008 image-store decision point. It does
//! **not** claim the OQ-008 decision is closed: actual image decoding,
//! placement, animation, and renderer coupling remain deferred pending the
//! image protocol RFC with security review. Until that RFC lands this
//! crate holds only bounded, headless-testable placeholders that observe
//! terminal truth without mutating it.
//!
//! # Drift and honesty statement
//!
//! - `bitty-docs/docs/interfaces/rich-content.md` is **draft** (not
//!   accepted). It sketches `RichBlock`, `SceneNode`, `SemanticZone`,
//!   and structured-transport candidates that are not implemented here.
//!   This crate does not copy those sketches as API; it interprets the
//!   already-implemented terminal-truth surfaces (snapshot cells +
//!   hyperlink ids, `State::zones`, `ImageStore`) and exposes presentation
//!   helpers that remain useful when a future rich-block model is chosen.
//! - `bitty-docs/docs/specifications/rich-content.md` does **not exist**
//!   (checked at task execution). The only canonical rich-content source
//!   is the draft under `interfaces/`.
//! - OQ-008 decision remains **open**. The image RFC has not landed;
//!   `bitty-term-state::ImageStore` is the canonical image-store seam
//!   (see `bitty_term_state::image`) and this crate's [`kitty`] module
//!   mirrors its bounds (64 placeholders, 4096 bytes each, oldest eviction).
//!   No pixel decode, no GPU allocation, no placement semantics, and no
//!   fallback chain exist yet.
//! - ADR-0003 ("Core Workspace Topology") does not list `bitty-rich` in
//!   its crate graph. This crate is proposed as a **draft presentation
//!   sibling** to `bitty-render` / `bitty-ui`, consuming only the public
//!   `Snapshot`/`Damage`/`Cell`/`ZoneRecord`/`ImageStore` surface. Its
//!   eventual placement (here, inside `bitty-render`, or as a `bitty-image`
//!   sibling per OQ-008 open question) will be decided when the image and
//!   rich-presentation RFCs are accepted.
//!
//! # Bounds (threat T-01)
//!
//! Every collection is bounded and deterministic:
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`hyperlink::HYPERLINK_TABLE_MAX`] (via term-state) | 1024 | new distinct link degrades to no link |
//! | [`shell::SHELL_ZONE_MAX`] mirrors `ZONE_RECORDS_MAX` | 1024 | oldest dropped |
//! | [`clipboard::CLIPBOARD_MAX_HISTORY`] | 16 | oldest dropped |
//! | [`clipboard::CLIPBOARD_MAX_PAYLOAD_BYTES`] | 4096 | truncation at cap |
//! | [`kitty::KITTY_MAX_PLACEHOLDERS`] via `IMAGE_STORE_MAX_ENTRIES` | 64 | oldest evicted |
//! | [`kitty::KITTY_MAX_PAYLOAD_BYTES`] via `IMAGE_STORE_MAX_PAYLOAD_BYTES` | 4096 | truncation |
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
pub mod kitty;
pub mod presentation;
pub mod shell;

pub use clipboard::{ClipboardPolicy, ClipboardRequest, ClipboardState};
pub use geometry::{CellMetrics, ExtentPx, RectPx};
pub use hyperlink::{HyperlinkInfo, HyperlinkSpan};
pub use kitty::{KittyGraphicsStub, KittyPlaceholder, KittyPlaceholderId};
pub use shell::{CommandRegion, ShellIntegration};
