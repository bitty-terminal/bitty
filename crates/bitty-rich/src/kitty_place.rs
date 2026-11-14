//! Kitty placement: decoded images onto cell rects (CTX-0248, W-141).
//!
//! Bounded Kitty `APC G` intake (chunked `m=` assembly) is owned by
//! `bitty-vt`'s `kitty_apc`; the retired `bitty-rich` placeholder stub is
//! not part of this path (bitty#2358). The bounded Kitty payload decoder
//! used to live here in `kitty_decode` (W-141 extraction): it now lives in
//! the graphics extension crate behind the Core-owned
//! [`crate::kitty_seam`] traits, which mirrors texture-preparation mechanics
//! (nearest-neighbor scaling, the per-frame blit budget, its own raster
//! cache copy); Core owns its raster cache in [`crate::kitty_raster`]
//! (S6, #1849).
//! This module is the Core-retained placement-policy half: it stores
//! caller-supplied decoded bitmaps (validated by `checked_bitmap`
//! before admission), binds them to cursor-anchored cell rects, and derives
//! the pixel rects the present layer composites. Parser/APC wiring is
//! unchanged: the caller passes the transmission parameters (`a`, `c`, `r`)
//! alongside the decoded bitmap.
//!
//! Core keeps the pre-allocation checker ([`precheck_declared_image`]) over
//! declared wire dimensions plus the pre-upload re-validator
//! (via `checked_bitmap`) regardless of the extension-side copies: a
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
//! - Relative placements (`a=p` with `P=`/`Q=` plus `H=`/`V=` offsets):
//!   anchored to the parent's *painted* origin plus those cell offsets,
//!   recomputed every frame by
//!   [`KittyImageLayer::placement_full_rect_resolved`]. The child therefore
//!   scrolls with its parent, hides when the parent leaves the grid or dies,
//!   and a missing/virtual/cyclic parent chain paints nothing (the walk is
//!   depth-bounded). `z=` stays the child's own, so a negative-`z` child
//!   still composites below text.
//! - Size: explicit `c`/`r` cell spans when non-zero, otherwise derived from
//!   decoded pixels (`ceil(px / cell)`, at least 1 cell per axis).
//! - The cell rect maps to pixels through the caller-supplied
//!   [`crate::geometry::CellMetrics`] and is clamped (intersected) to the
//!   viewport; a placement fully outside the viewport paints nothing.
//! - Z-order (documented model): grid cells (backgrounds + glyphs) and every
//!   fill overlay (selection, cursor, banner, scrollbar) composite first;
//!   kitty images with non-negative `z` composite above cells, text, and
//!   selection fills, ordered by ascending `z` (stable for equal `z`), but
//!   below the focused cursor (#1849 S7 cursor-on-top). A negative `z`
//!   (kitty layering rule) instead composites above cell backgrounds and
//!   below text, so glyphs and overlay fills paint over the image. They
//!   never mutate grid truth.
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
//! viewport), as is the per-screen plane swap (see below).
//!
//! Limitation: scroll regions (`DECSTBM`) and reverse-index moves that do
//! not grow the scrollback are not tracked, so an image inside a scroll
//! region may lag its text by the region-local delta. Full-screen scroll
//! (the common case) is exact.
//!
//! # Alternate screen
//!
//! Kitty keeps a separate image plane per screen. The runtime owns the
//! swap: on alternate-screen entry it detaches this origin's main entries
//! with [`KittyImageLayer::take_placements_for`] /
//! [`KittyImageLayer::take_virtuals_for`], alt displays register live, and
//! on leave it clears the alt entries and re-attaches main with
//! [`KittyImageLayer::restore_placements`] /
//! [`KittyImageLayer::restore_virtuals`]. Descriptors are therefore absent
//! (not painted, not discarded) while their plane is dormant.
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
//! Per-origin quotas (S5, #1849, CTX-1093).
//!
//! The shipped global caps below are partitioned per origin: every stored
//! image carries the transmitting stream's origin token
//! ([`KittyPlacedImage::origin`]) and counts against that origin's 64-image
//! / 64 MiB quotas ([`KITTY_PER_ORIGIN_MAX_IMAGES`] /
//! [`KITTY_PER_ORIGIN_MAX_BYTES`]); every placement counts against its
//! origin's 32-placement quota ([`KITTY_PER_ORIGIN_MAX_PLACEMENTS`]). A
//! noisy origin therefore evicts only its own oldest entries (FIFO within
//! the origin, deterministic), and global pressure from other origins
//! refuses the new admission ([`KittyPlacementError::QuotaExceeded`])
//! instead of evicting a victim — no cross-origin eviction. Dangling
//! placements (image refused or evicted) fail closed at lookup and paint
//! nothing, as before. Origin count stays bounded by the live pane-session
//! count plus the primary grid, and accounting scans the bounded deques,
//! so quotas add no allocation and the global totals never exceed the
//! shipped bounds.
//!
//! Residual posture (documented, not enforced here):
//!
//! - Within one origin, images composite above that pane's own selection
//!   fills but below its focused cursor (#1849 S7 cursor-on-top; see
//!   "Placement semantics" above). Same-origin impact
//!   is contained: the emitting program already controls every cell of its
//!   own grid, so covering its own chrome adds no new spoof capability
//!   beyond what PTY text already allows.
//!
//! # Bounds (threat T-01/T-02)
//!
//! Placement enforces the Core-retained decode ceilings
//! ([`KITTY_DECODE_MAX_DIMENSION`]/[`KITTY_DECODE_MAX_PIXELS`]/[`KITTY_DECODE_MAX_BYTES`]:
//! 8192 px/side, 4096 x 4096 px area, 64 MiB RGBA) rather than
//! inventing its own. These are Core policy constants mirroring the
//! versioned graphics contract; the graphics extension holds its
//! own copies and receives limits per request, never via a Core import
//! (one-way dependencies). Every
//! length is validated with checked arithmetic **before** any buffer is
//! allocated or grown, including the viewport-clamped blit size
//! (`rect_w * rect_h * 4`, itself bounded because the rect is clamped to
//! the viewport first). Layer totals are additionally capped: 64 stored
//! images ([`KITTY_PLACE_MAX_IMAGES`], kitty-ledger parity) and 256 MiB
//! decoded bytes ([`KITTY_PLACE_MAX_BYTES`], RFC IMG-4 parity), partitioned
//! per origin at 64 images / 64 MiB ([`KITTY_PER_ORIGIN_MAX_IMAGES`] /
//! [`KITTY_PER_ORIGIN_MAX_BYTES`]) with FIFO eviction within the admitting
//! origin only; placements are capped at 128 globally
//! ([`KITTY_PLACE_MAX_ITEMS`], RFC IMG-8 parity) and 32 per origin
//! ([`KITTY_PER_ORIGIN_MAX_PLACEMENTS`]), oldest-of-origin evicted first.
//! Global pressure from other origins refuses the admission
//! ([`KittyPlacementError::QuotaExceeded`]) instead of evicting a victim.
//! A single image larger than the byte cap is rejected.
//!
//! # Per-frame budget (CTX-0252 F2, policy half retained)
//!
//! The present loop composites at most [`KITTY_PRESENT_MAX_BLITS_PER_FRAME`]
//! blits / [`KITTY_PRESENT_MAX_BYTES_PER_FRAME`] bytes per frame
//! (skip-and-continue in paint order), so the pathological 128-placement
//! transient (128 x 64 MiB) can never materialize. The frame-budget
//! accounting type lives in the graphics extension with the rest
//! of the texture-preparation mechanics (which keeps its own cache copy);
//! Core owns its bounded raster cache alongside it
//! ([`crate::kitty_raster::KittyRasterCache`], S6 #1849, same 32 / 64 MiB
//! ceilings as here). The caps above stay Core-owned so the present layer
//! keeps enforcing the same ceilings the parity tests pin.
//!
//! CTX-1087 (F11, #1891) normative declaration: this module is the ONE
//! normative site for the Kitty present per-frame quotas (32 blits,
//! 64 MiB). `bitty-render`'s `MAX_IMAGE_BLITS_PER_FRAME` /
//! `MAX_IMAGE_UPLOAD_BYTES_PER_FRAME` mirror these values (see
//! `bitty-render/src/batch.rs`), and the graphics extension holds
//! its own `KITTY_PRESENT_MAX_*` copies received per request. Values here
//! must not change without updating the mirrors; this note declares truth,
//! no value changes.
//!
//! CTX-1087 placement-ownership note: this [`KittyImageLayer`] owns the
//! live presentation truth (decoded bitmaps, viewport pixel rects, paint
//! order); `bitty-term-state`'s `PlacementStore` owns the terminal-grid
//! truth (anchors, scroll, delete selectors, animation timing, and the
//! normative `KITTY_Z_BELOW_BACKGROUND` threshold this layer orders `z`
//! against). The stores are complementary, not duplicates; neither is
//! deleted. The demoted legacy seam is `bitty-term-state::image`'s
//! `ImagePlaceholder`.
//!
//! # Virtual prototypes (`U=1`, S4, #1849)
//!
//! Unicode-placeholder images (`U+10EEEE` runs) paint through the grid,
//! never as blits: the client transmits the image (quiet `a=t`, or the
//! combined `a=T,U=1` form) and registers a virtual prototype
//! (`a=p,U=1,i=<id>,c=<cols>,r=<rows>`, always bodiless — the parser
//! refuses payload on control-only actions), then prints placeholder
//! runs the present layer sizes via [`crate::kitty_unicode`]. This layer
//! keeps the prototype registry ([`KittyVirtualPrototype`], bounded by
//! [`KITTY_VIRTUAL_MAX_ITEMS`]) so runs resolve against registered spans:
//!
//! - Registration is origin-tagged with wire `i=`/`p=` identity and the
//!   `scrollback_base` at registration; scroll follows the same
//!   `current.saturating_sub(base)` delta as blit placements.
//! - Prototypes count against the shared placement budget (128 global /
//!   32 per origin): admission runs the same quota shape as blits, with
//!   same-kind FIFO eviction only (a prototype never evicts a visible
//!   blit and vice versa; a budget held entirely by the other kind
//!   refuses with [`KittyPlacementError::QuotaExceeded`]).
//! - Present never blits prototypes: they live outside the placements
//!   deque, so every paint-order iterator skips them by construction
//!   (no double-paint with the grid runs).
//! - Deletion mirrors terminal truth (`bitty-term-state::placement`):
//!   unpinned `d=i` clears every prototype of the image, pinned deletes
//!   spare them (any prototype still serves runs), pin `0` names nothing;
//!   `d=a` keeps them (`clear_origin` stays placements-only) and the
//!   alternate-screen plane swap detaches and re-attaches them with the
//!   plane ([`KittyImageLayer::take_virtuals_for`] /
//!   [`KittyImageLayer::restore_virtuals`]), while `ED 2`/`ED 22`/
//!   `FullReset` (`clear_virtual_for_origin`), pane teardown
//!   (`retire_origin`), and image eviction (linked combined-form
//!   prototypes cascade) drop them.
//! - Combined-form prototypes link their stored image (`Some` handle);
//!   bodiless prototypes carry `None` and explicit spans only (`0` sides
//!   derive `>= 1` cell, as in [`crate::kitty_unicode`]), and the present
//!   layer resolves their image through the origin-scoped wire map
//!   ([`KittyImageLayer::find_root_for_origin`]).
//!
//! # Determinism
//!
//! Storage and placement are pure functions of insertion order: same calls
//! always yield the same ids and the same retained set.
//!
//! # Animation frame store (`a=f` / `a=c` playback, S1, #1849)
//!
//! Terminal truth (`bitty-term-state::placement`) owns the animation
//! descriptors (frame gaps, current frame, run/stop state, loop budget);
//! this layer owns the frame *pixels*. Frame 1 is the root image (the
//! [`KittyPlacedImage::rgba`] bytes stored at transmit time); `a=f`
//! appends frames 2..=N as [`KittyFrame`] records and `a=c` blends or
//! replaces bounded rects between stored frames in place.
//!
//! Frame ingest reuses every shipped per-frame cap
//! ([`KITTY_DECODE_MAX_DIMENSION`]/[`KITTY_DECODE_MAX_PIXELS`]/[`KITTY_DECODE_MAX_BYTES`]:
//! 8192 px/side, 4096 x 4096 px area, 64 MiB RGBA) and adds two animation
//! totals enforced before any buffer grows: at most
//! [`KITTY_ANIM_MAX_FRAMES`] frames per image (root included) and at most
//! [`KITTY_ANIM_MAX_BYTES_PER_IMAGE`] decoded bytes per image (64 MiB,
//! the per-frame ceiling applied to the whole animation). The global
//! [`KITTY_PLACE_MAX_BYTES`] (256 MiB) bound covers frame bytes too, so a
//! frame ingest that would overflow it refuses with
//! [`KittyPlacementError::QuotaExceeded`]. Frame ingest never evicts:
//! refusals store nothing and evict nothing (evicting whole images to make
//! room for one frame would drop other animations' pixels mid-playback).
//! Every frame must match the root frame's width and height exactly
//! ([`KittyPlacementError::FrameDimensionsMismatch`] otherwise): the
//! raster step samples every frame at the root extent, and mixed-size
//! frames would alias the cache key.
//!
//! Frame-count reconciliation (CTX-1090 open question 2, decided): the
//! generic image store caps animations at 64 frames
//! (`IMAGE_MAX_FRAMES`), and presentation follows it — but terminal truth
//! allows 256 gap entries per descriptor (cheap `u32`s, no pixels). Past
//! 64 stored frames ingest refuses with
//! [`KittyPlacementError::TooManyFrames`]; the runtime records the truth
//! gap only on successful ingest (store-first), so the two counts agree
//! and the 256-gap ceiling is unreachable through the pixel path —
//! playback still clamps the truth current frame into the stored range,
//! and a truth frame with no stored pixels paints nothing (fail closed,
//! same shape as a dangling placement), as defense-in-depth.
//!
//! `a=c` compose (CTX-1090 open question 3, decided): the wire rect size
//! (`w=`, `h=`, defaulting to the full frame extent) applies to two
//! independent positions — source frame `r=` at (`X=`, `Y=`) and
//! destination frame `c=` at (`x=`, `y=`) — and each rectangle must lie
//! inside the frame bounds with checked `u64` arithmetic, else the compose
//! refuses with [`KittyPlacementError::ComposeOutOfBounds`] and composes
//! nothing (this includes empty rects and rect-end overflow past
//! `u32::MAX`).
//! `C=0` alpha-blends the source over the destination (straight-alpha
//! src-over, rounded integer math, pinned by test); `C=1` replaces the
//! rect bytes outright; any other mode refuses with
//! [`KittyPlacementError::UnknownComposeMode`]. When source and
//! destination name the same frame and the rectangles overlap, the
//! compose refuses with
//! [`KittyPlacementError::ComposeSelfOverlap`] and composes nothing
//! (S1 spec follow-up, CTX-1124: kitty
//! `docs/graphics-protocol.rst:1024-1027`,
//! `kitty/graphics.c:2077-2083` — `MAX(src,dst) < MIN(src,dst)+size` on
//! both axes; identical rects and partial overlaps refuse, disjoint
//! including edge-touching still composes).
//! A successful compose bumps the image generation (the cache key binds
//! it) and the runtime drops cached blits wholesale, so no stale frame
//! bytes can survive either path.
//!
//! `a=f` partial frames: wire data smaller than the root extent (or sent
//! with an `x=`/`y=` offset) composites onto a background canvas before
//! ingest — wire `c=` base frame when present, else the wire `Y=` 32-bit
//! RGBA color (transparent black when absent) — with the wire `X=`
//! composition mode. The composed full-extent frame then ingests through
//! the same quota-checked path as full frames, so a valid partial frame
//! with a `c=` base stores instead of tripping
//! [`KittyPlacementError::FrameDimensionsMismatch`].
//!
//! `a=f` edits (`r>0`): the wire extent composites onto the existing frame
//! itself in place — the background canvas is the edited frame, never the
//! wire `c=` base or `Y=` color — with the wire `X=` composition mode at
//! the wire `x=`/`y=` offset. Missing frames, unknown modes, and
//! out-of-bounds rects refuse with nothing edited; byte totals are
//! unchanged, so no quota can trip.
//!
//! Wire identity: every stored image carries the transmitting command's
//! wire `i=` id ([`KittyPlacedImage::wire_image`], `0` anonymous) scoped
//! by its origin, so `a=f`/`a=c` resolve their target without touching
//! placements — transmit-only roots (never placed) still gain frames.
//! Resolution takes the newest image of `(origin, wire id)`; anonymous
//! (`0`) wire ids never resolve. Virtual (`U=1`) roots tag the same way,
//! so virtual animations ingest through the identical path while painting
//! stays grid-run-only (the S4 double-paint rule is unchanged).

use std::collections::VecDeque;

use crate::geometry::{CellMetrics, ExtentPx, RectPx};
use crate::kitty_unicode::{KittyUnicodeRect, KittyUnicodeVirtual, unicode_run_rect};
use bitty_term_state::{KittyUnicodeCell, KittyUnicodeRun};

/// Maximum parent-chain depth walked when resolving a relative placement
/// (issue #2875).
///
/// The wire lets a client re-put an existing placement with a new `P=`/`Q=`
/// parent, so a cycle (`p1 → p2 → p1`) is reachable even though terminal
/// truth refuses a missing parent: truth's cascade only drops *orphans*, and
/// every node of a cycle has a parent. The resolver therefore bounds the walk
/// and fails closed past this depth instead of looping. Eight levels cover
/// every sane composite (a parent plus a few nested overlays) while keeping
/// the walk O(depth × placements), bounded with the placement caps.
pub const KITTY_PLACE_MAX_PARENT_DEPTH: u8 = 8;

/// Kitty `f=` value for PNG payloads (Core-retained contract mirror).
///
/// The bounded decoder moved to the graphics extension (W-141),
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
///
/// Aliases the legacy terminal-truth placeholder ceiling
/// (`bitty_term_state::IMAGE_STORE_MAX_ENTRIES`, 64). The retired
/// `bitty-rich` kitty stub used to be the single source of this alias; the
/// placement layer now owns it directly with the value unchanged
/// (bitty#2358).
pub const KITTY_PLACE_MAX_IMAGES: usize = bitty_term_state::IMAGE_STORE_MAX_ENTRIES;

/// Maximum total bytes per kitty ledger: resident images plus the in-flight
/// chunk buffer, Ghostty `total_limit` parity (`320 * 1000 * 1000`).
///
/// Core-retained backstop for the kitty intake path. It bounds any future
/// intake layer that stores or streams images; the retired placeholder stub
/// used to define it (bitty#2358), and the decode path's compile-time
/// headroom assertion below keeps its value load-bearing.
pub const KITTY_LEDGER_MAX_BYTES: usize = 320 * 1000 * 1000;

/// Maximum total decoded RGBA bytes held by the layer (RFC IMG-4 parity).
pub const KITTY_PLACE_MAX_BYTES: usize = crate::image::IMAGE_STORE_MAX_BYTES;

/// Maximum placements (RFC IMG-8 parity).
pub const KITTY_PLACE_MAX_ITEMS: usize = crate::image::IMAGE_MAX_PLACEMENTS;

/// Maximum stored decoded images per origin (S5 per-origin quota, #1849).
///
/// Keeps kitty-ledger parity (64) so single-origin count behavior is
/// unchanged: the count cap only binds one origin at a time, while the
/// byte quota below is the binding partition for large bitmaps.
pub const KITTY_PER_ORIGIN_MAX_IMAGES: usize = 64;

/// Maximum decoded RGBA bytes held per origin (S5 per-origin quota, #1849).
///
/// Quarters the 256 MiB global bound so up to four full origins fit inside
/// it. The 64 MiB single-bitmap decode ceiling exactly fills one origin
/// quota: a maximal image is still admittable, evicting that origin's own
/// older images first.
pub const KITTY_PER_ORIGIN_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Maximum placements per origin (S5 per-origin quota, #1849).
///
/// Quarters the 128 global bound so up to four full origins fit inside it.
/// The per-frame blit budget (32 blits) already means placements past one
/// origin quota rarely paint on a single pane in one frame.
pub const KITTY_PER_ORIGIN_MAX_PLACEMENTS: usize = 32;

/// Maximum animation frames per image, root frame included (S1, #1849).
///
/// `IMAGE_MAX_FRAMES` parity (64): the generic image store's IMG-6 bound.
/// Terminal truth holds up to 256 gap entries per descriptor (cheap
/// `u32`s); presentation stores pixels for at most these 64 — see the
/// module-level frame-count reconciliation note.
pub const KITTY_ANIM_MAX_FRAMES: usize = crate::image::IMAGE_MAX_FRAMES as usize;

/// Maximum total decoded bytes per animated image (S1, #1849): the
/// per-frame ceiling ([`KITTY_DECODE_MAX_BYTES`], 64 MiB) applied to the
/// whole animation (root plus every appended frame). A single maximal
/// frame already fills it; 64 one-MiB frames fit exactly.
pub const KITTY_ANIM_MAX_BYTES_PER_IMAGE: usize = KITTY_DECODE_MAX_BYTES;

/// Compose mode `C=0` (S1, #1849): alpha-blend the source rect over the
/// destination rect (straight-alpha src-over).
pub const KITTY_COMPOSE_BLEND: u32 = 0;

/// Compose mode `C=1` (S1, #1849): replace the destination rect bytes
/// with the source rect bytes outright.
pub const KITTY_COMPOSE_REPLACE: u32 = 1;

/// Maximum virtual prototypes retained by the registry (S4, #1849).
///
/// Mirrors the terminal-truth [`bitty_term_state`] placement capacity so
/// every recorded prototype has a registry slot. The shared 128-global
/// placement budget subsumes it in practice (prototypes count against
/// that budget); this stays as the absolute backstop.
pub const KITTY_VIRTUAL_MAX_ITEMS: usize = 256;

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
///
/// Frame 1 (the root) lives in [`Self::rgba`]; animation frames 2..=N
/// (S1, #1849) live in [`Self::frames`]. See the module-level animation
/// frame store note for the bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyPlacedImage {
    /// Stable handle.
    pub id: KittyImageId,
    /// Content generation for the raster-cache key (S6, #1849): assigned
    /// from a monotonic store sequence at admission and never mutated
    /// afterwards. The cache key binds `(id, generation)` instead of a
    /// bare id so a future id reuse (counter wrap, currently only
    /// theoretical at 2^64 stores) can never serve stale bytes: a reused
    /// id carries a fresh generation and misses. Stored bitmaps are
    /// otherwise immutable; a future in-place update path must bump this.
    ///
    /// S1 (`a=c` compose, #1849) is that update path: a successful compose
    /// bumps the generation (the cache key binds it, and the runtime drops
    /// cached blits wholesale on top), so edited frames never serve stale
    /// bytes.
    pub generation: u64,
    /// Origin token of the transmitting stream (S5 per-origin quotas,
    /// #1849): `None` for the primary grid, `Some(token)` for a split-pane
    /// session. The image counts against this origin's store quota and is
    /// only ever evicted for pressure from the same origin, never for
    /// another origin's admissions.
    pub origin: Option<u64>,
    /// Wire `i=` image id naming this image (S1 animation target, #1849):
    /// `0` when absent (anonymous). Scoped by [`Self::origin`]:
    /// `a=f`/`a=c` resolve `(origin, wire_image)` to the newest stored
    /// image, so transmit-only roots (never placed) still gain frames.
    /// Anonymous images never resolve as animation targets.
    pub wire_image: u32,
    /// Decoded pixel width.
    pub width: u32,
    /// Decoded pixel height.
    pub height: u32,
    /// RGBA8 bytes, exactly `width * height * 4` long.
    pub rgba: Vec<u8>,
    /// Animation frames 2..=N (S1, #1849), oldest first. Frame 1 is the
    /// root ([`Self::rgba`]); every entry matches the root width and
    /// height exactly (enforced at ingest).
    pub frames: Vec<KittyFrame>,
    /// Wire payload length (diagnostics; the bytes themselves are decoded).
    pub compressed_len: usize,
}

impl KittyPlacedImage {
    /// Total decoded bytes held by this image: root plus every appended
    /// frame, summed with saturation so hostile lengths can never wrap
    /// the accounting itself.
    #[must_use]
    pub fn stored_bytes(&self) -> usize {
        self.frames.iter().fold(self.rgba.len(), |acc, frame| {
            acc.saturating_add(frame.rgba.len())
        })
    }

    /// Frame count including the root frame (always `>= 1`).
    #[must_use]
    pub fn frame_count(&self) -> u32 {
        u32::try_from(self.frames.len().saturating_add(1)).unwrap_or(u32::MAX)
    }
}

/// One stored animation frame beyond the root (S1, #1849).
///
/// Same row-major straight-alpha RGBA8 shape as the root; width and height
/// always equal the root's (enforced at ingest, re-checked fail-closed by
/// the raster step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyFrame {
    /// Decoded pixel width (always the root width).
    pub width: u32,
    /// Decoded pixel height (always the root height).
    pub height: u32,
    /// RGBA8 bytes, exactly `width * height * 4` long.
    pub rgba: Vec<u8>,
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
    /// Wire `(P=, Q=)` parent identity for a relative placement (issue
    /// #2875): the parent's origin-scoped wire `(image id, placement id)`.
    /// `None` for an absolutely anchored placement. A relative placement's
    /// own [`Self::anchor_col`]/[`Self::anchor_row`] are unused: the paint
    /// path derives its rect from the parent's *painted* origin at paint
    /// time ([`KittyImageLayer::placement_full_rect_resolved`]), so scroll
    /// needs no second bookkeeping here and a dead parent simply paints
    /// nothing.
    pub parent: Option<(u32, u32)>,
    /// Wire `H=`/`V=` cell offset from the parent's painted origin as
    /// `(dx, dy)`; `(0, 0)` for absolute placements.
    pub parent_offset: (i32, i32),
}

/// Virtual-prototype record for one `U=1` registration (S4, #1849).
///
/// Invisible bookkeeping for `U+10EEEE` runs: the prototype names the
/// image and the cell span the client's runs will cover, but it is never
/// anchored, never moves the cursor, and never emits a blit (prototypes
/// live outside the placements deque, so paint-order iterators skip them
/// by construction — the double-paint rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KittyVirtualPrototype {
    /// Origin token of the emitting PTY stream (`None` primary,
    /// `Some(token)` pane session): quota scope and lookup scope, like
    /// [`KittyPlacement::origin`].
    pub origin: Option<u64>,
    /// Wire `i=` image id naming the prototype (`0` anonymous).
    pub wire_image: u32,
    /// Wire `p=` placement id (`0` anonymous: any-placement fallback,
    /// never singly addressable).
    pub wire_placement: u32,
    /// Linked stored image for the combined `a=T,U=1` form (`None` for
    /// bodiless `a=p,U=1`: that form carries no pixels, so the present
    /// layer resolves the image through the origin-scoped wire map at
    /// paint time — canonical `a=t` then `a=p,U=1`).
    pub image: Option<KittyImageId>,
    /// Explicit `c=` span columns (`0` derives, see below).
    pub cols: u16,
    /// Explicit `r=` span rows (`0` derives, see below).
    pub rows: u16,
    /// Decoded image width in pixels (`0` when bodiless/unknown).
    pub image_width: u32,
    /// Decoded image height in pixels (`0` when bodiless/unknown).
    pub image_height: u32,
    /// `State::scrollback_len()` at registration (scroll tracking, same
    /// delta shape as [`KittyPlacement::scrollback_base`]).
    pub scrollback_base: usize,
}

impl KittyVirtualPrototype {
    /// Sizing view for [`unicode_run_rect`]: wire `p=0` maps to absent
    /// (any virtual placement of the image may serve the run).
    #[must_use]
    pub fn as_unicode_virtual(&self) -> KittyUnicodeVirtual {
        KittyUnicodeVirtual {
            image_id: self.wire_image,
            placement_id: if self.wire_placement == 0 {
                None
            } else {
                Some(self.wire_placement)
            },
            cols: self.cols,
            rows: self.rows,
            image_width: self.image_width,
            image_height: self.image_height,
        }
    }
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
    /// Animation frame ingest refused: the image already holds
    /// [`KITTY_ANIM_MAX_FRAMES`] frames (root included). Stores nothing
    /// and evicts nothing.
    TooManyFrames {
        /// Frames already held (root included).
        count: usize,
        /// Frame cap that refused the append.
        cap: usize,
    },
    /// Animation frame ingest refused: the new frame would push the
    /// image's decoded total past [`KITTY_ANIM_MAX_BYTES_PER_IMAGE`].
    /// Stores nothing and evicts nothing.
    AnimationTooLarge {
        /// Bytes the animation would have held.
        total: usize,
        /// Per-image byte cap that refused the append.
        cap: usize,
    },
    /// Animation frame ingest refused: the frame's dimensions differ from
    /// the root frame's. Every frame of one image shares the root extent
    /// (the raster step samples at the root size). Stores nothing.
    FrameDimensionsMismatch {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Root width the frame had to match.
        root_width: u32,
        /// Root height the frame had to match.
        root_height: u32,
    },
    /// Compose refused: `dest` or `src` names a frame with no stored
    /// pixels (1-based; frame 1 is the root). Composes nothing.
    NoSuchFrame {
        /// Image the compose addressed.
        image: KittyImageId,
        /// Missing 1-based frame number.
        frame: u32,
    },
    /// Compose refused: the rect lies outside the frame bounds, is empty,
    /// or its end overflows `u32`. Composes nothing.
    ComposeOutOfBounds {
        /// Rect left edge (`x=`).
        x: u32,
        /// Rect top edge (`y=`).
        y: u32,
        /// Rect width (`w=`).
        w: u32,
        /// Rect height (`h=`).
        h: u32,
        /// Frame width the rect had to fit.
        width: u32,
        /// Frame height the rect had to fit.
        height: u32,
    },
    /// Compose refused: the `X=` mode is neither
    /// [`KITTY_COMPOSE_BLEND`] (0) nor [`KITTY_COMPOSE_REPLACE`] (1).
    /// Composes nothing.
    UnknownComposeMode(u32),
    /// Compose refused: source and destination frames are the same and
    /// the rectangles overlap (S1 spec follow-up, CTX-1124). Composes
    /// nothing.
    ComposeSelfOverlap {
        /// Source left edge (`X=`).
        src_x: u32,
        /// Source top edge (`Y=`).
        src_y: u32,
        /// Destination left edge (`x=`).
        dst_x: u32,
        /// Destination top edge (`y=`).
        dst_y: u32,
        /// Rect width (`w=`, defaulted to the frame extent).
        w: u32,
        /// Rect height (`h=`, defaulted to the frame extent).
        h: u32,
    },
    /// Admission would exceed a global bound held by other origins.
    ///
    /// S5 per-origin quotas (#1849): eviction never crosses origins, so
    /// when the admitting origin is within its own quota but the global
    /// image, byte, or placement bound is already held by other origins,
    /// the admission is refused instead of evicting a victim. Failures
    /// store and place nothing.
    QuotaExceeded,
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
            Self::TooManyFrames { count, cap } => write!(
                f,
                "kitty animation of {count} frames exceeds max of {cap} frames"
            ),
            Self::AnimationTooLarge { total, cap } => write!(
                f,
                "kitty animation of {total} bytes exceeds max of {cap} bytes per image"
            ),
            Self::FrameDimensionsMismatch {
                width,
                height,
                root_width,
                root_height,
            } => write!(
                f,
                "kitty animation frame {width}x{height} does not match root {root_width}x{root_height}"
            ),
            Self::NoSuchFrame { image, frame } => write!(
                f,
                "kitty compose frame {frame} not held by image {}",
                image.0
            ),
            Self::ComposeOutOfBounds {
                x,
                y,
                w,
                h,
                width,
                height,
            } => write!(
                f,
                "kitty compose rect {w}x{h} at ({x},{y}) outside frame {width}x{height}"
            ),
            Self::UnknownComposeMode(mode) => {
                write!(f, "kitty compose unknown mode C={mode}")
            }
            Self::ComposeSelfOverlap {
                src_x,
                src_y,
                dst_x,
                dst_y,
                w,
                h,
            } => write!(
                f,
                "kitty compose overlapping self-compose {w}x{h} src ({src_x},{src_y}) dst ({dst_x},{dst_y}) on same frame"
            ),
            Self::QuotaExceeded => write!(
                f,
                "kitty quota exceeded: global bound held by other origins, refused without evicting"
            ),
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

/// Fits an image into `outer` preserving aspect ratio, centered on both
/// axes (kitty `docs/graphics-protocol.rst`: "When both are specified the
/// image is letterboxed/pillarboxed to prevent distortion").
///
/// The returned rectangle keeps `outer`'s origin plus the centered inset,
/// so callers can scale the whole image into it and crop the visible
/// window without re-scaling (the `#1334` rule). A zero image axis or an
/// empty `outer` returns `outer` unchanged (nothing to fit).
#[must_use]
pub fn fit_aspect_rect(outer: RectPx, image_width: u32, image_height: u32) -> RectPx {
    if outer.is_empty() || image_width == 0 || image_height == 0 {
        return outer;
    }
    let ow = u64::from(outer.width);
    let oh = u64::from(outer.height);
    let iw = u64::from(image_width);
    let ih = u64::from(image_height);
    // Scale by width when the width constraint is tighter, else by height.
    // Integer math only; the `min` guards the ceil-rounded span edges.
    let (fw, fh) = if ow.saturating_mul(ih) <= oh.saturating_mul(iw) {
        (ow, (ih.saturating_mul(ow) / iw).clamp(1, oh))
    } else {
        ((iw.saturating_mul(oh) / ih).clamp(1, ow), oh)
    };
    let inset_x = u64::from(outer.width).saturating_sub(fw) / 2;
    let inset_y = u64::from(outer.height).saturating_sub(fh) / 2;
    let inset_x = i64::try_from(inset_x).unwrap_or(i64::MAX);
    let inset_y = i64::try_from(inset_y).unwrap_or(i64::MAX);
    let x = i64::from(outer.x).saturating_add(inset_x);
    let y = i64::from(outer.y).saturating_add(inset_y);
    let to_i32 = |v: i64| i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX });
    RectPx::new(
        to_i32(x),
        to_i32(y),
        u32::try_from(fw).unwrap_or(u32::MAX),
        u32::try_from(fh).unwrap_or(u32::MAX),
    )
}

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// Owned Kitty image + placement layer (headless, bounded).
///
/// Deterministic per-origin FIFO eviction (S5, #1849): each origin is
/// capped at [`KITTY_PER_ORIGIN_MAX_IMAGES`] images,
/// [`KITTY_PER_ORIGIN_MAX_BYTES`] bytes, and
/// [`KITTY_PER_ORIGIN_MAX_PLACEMENTS`] placements inside the global bounds,
/// and pressure evicts that origin's own oldest entries first. Same call
/// order always yields the same ids and the same retained set.
#[derive(Debug, Clone, Default)]
pub struct KittyImageLayer {
    images: VecDeque<KittyPlacedImage>,
    placements: VecDeque<KittyPlacement>,
    virtual_protos: VecDeque<KittyVirtualPrototype>,
    total_bytes: usize,
    next_image_id: u64,
    next_placement_id: u64,
    next_image_generation: u64,
}

impl KittyImageLayer {
    /// An empty layer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            images: VecDeque::new(),
            placements: VecDeque::new(),
            virtual_protos: VecDeque::new(),
            total_bytes: 0,
            next_image_id: 1,
            next_placement_id: 1,
            next_image_generation: 1,
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

    /// Number of stored decoded images of one origin (S5, #1849).
    #[must_use]
    pub fn image_count_for_origin(&self, origin: Option<u64>) -> usize {
        self.images
            .iter()
            .filter(|img| img.origin == origin)
            .count()
    }

    /// Decoded RGBA bytes held by one origin (S5, #1849).
    ///
    /// Summed with saturating arithmetic over the bounded store, so the
    /// accounting itself can never overflow on hostile inputs. S1 (#1849):
    /// covers root plus every appended animation frame.
    #[must_use]
    pub fn bytes_for_origin(&self, origin: Option<u64>) -> usize {
        self.images
            .iter()
            .filter(|img| img.origin == origin)
            .fold(0usize, |acc, img| acc.saturating_add(img.stored_bytes()))
    }

    /// Number of retained placements of one origin (S5, #1849).
    #[must_use]
    pub fn placement_count_for_origin(&self, origin: Option<u64>) -> usize {
        self.placements
            .iter()
            .filter(|p| p.origin == origin)
            .count()
    }

    /// Number of retained placements.
    #[must_use]
    pub fn placement_len(&self) -> usize {
        self.placements.len()
    }

    /// Number of registered virtual prototypes (S4, #1849).
    #[must_use]
    pub fn virtual_len(&self) -> usize {
        self.virtual_protos.len()
    }

    /// Number of registered virtual prototypes of one origin (S4, #1849).
    #[must_use]
    pub fn virtual_count_for_origin(&self, origin: Option<u64>) -> usize {
        self.virtual_protos
            .iter()
            .filter(|v| v.origin == origin)
            .count()
    }

    /// Looks up a virtual prototype by origin-scoped wire identity (S4,
    /// #1849): `None` placement matches the oldest prototype of the
    /// image on `origin` (any-placement fallback); `Some(pin)` needs the
    /// exact `(image, pin)` pair. `Some(0)` names nothing singly
    /// addressable and returns `None` (mirrors [`Self::delete_by_wire`]).
    #[must_use]
    pub fn get_virtual(
        &self,
        origin: Option<u64>,
        image_id: u32,
        placement: Option<u32>,
    ) -> Option<&KittyVirtualPrototype> {
        match placement {
            Some(0) => None,
            Some(pinned) => self.virtual_protos.iter().find(|v| {
                v.origin == origin && v.wire_image == image_id && v.wire_placement == pinned
            }),
            None => self
                .virtual_protos
                .iter()
                .find(|v| v.origin == origin && v.wire_image == image_id),
        }
    }

    /// Registers a virtual prototype for one origin (S4, #1849).
    ///
    /// An addressed re-put (`i != 0`, `p != 0`) of an already-registered
    /// `(origin, image, placement)` key replaces in place (terminal-truth
    /// `upsert` parity: no quota consumed, nothing evicted); anything
    /// else appends. Appends count against the shared placement budget
    /// (128 global / 32 per origin): pressure evicts that origin's own
    /// oldest prototype first (FIFO within the origin, deterministic),
    /// and global pressure from other origins refuses with
    /// [`KittyPlacementError::QuotaExceeded`] instead of evicting a
    /// victim — no cross-origin eviction. Same-kind FIFO only: a budget
    /// held entirely by blit placements refuses (a prototype never
    /// evicts visible content). The absolute [`KITTY_VIRTUAL_MAX_ITEMS`]
    /// bound backstops the registry. Failures register nothing and evict
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::QuotaExceeded`] on global pressure from
    /// other origins, or when the origin's budget is held entirely by
    /// blit placements.
    #[allow(clippy::too_many_arguments)]
    pub fn register_virtual_for_origin(
        &mut self,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
        image: Option<KittyImageId>,
        cols: u16,
        rows: u16,
        image_width: u32,
        image_height: u32,
        scrollback_base: usize,
    ) -> Result<(), KittyPlacementError> {
        if wire_image != 0 && wire_placement != 0 {
            if let Some(slot) = self.virtual_protos.iter_mut().find(|v| {
                v.origin == origin
                    && v.wire_image == wire_image
                    && v.wire_placement == wire_placement
            }) {
                *slot = KittyVirtualPrototype {
                    origin,
                    wire_image,
                    wire_placement,
                    image,
                    cols,
                    rows,
                    image_width,
                    image_height,
                    scrollback_base,
                };
                return Ok(());
            }
        }
        self.check_virtual_quota_for_origin(origin)?;
        if self.placement_count_for_origin(origin) + self.virtual_count_for_origin(origin)
            >= KITTY_PER_ORIGIN_MAX_PLACEMENTS
        {
            if let Some(pos) = self.virtual_protos.iter().position(|v| v.origin == origin) {
                self.virtual_protos.remove(pos);
            }
        }
        if self.virtual_protos.len() >= KITTY_VIRTUAL_MAX_ITEMS {
            if let Some(pos) = self.virtual_protos.iter().position(|v| v.origin == origin) {
                self.virtual_protos.remove(pos);
            } else {
                return Err(KittyPlacementError::QuotaExceeded);
            }
        }
        self.virtual_protos.push_back(KittyVirtualPrototype {
            origin,
            wire_image,
            wire_placement,
            image,
            cols,
            rows,
            image_width,
            image_height,
            scrollback_base,
        });
        Ok(())
    }

    /// Resolves one grid run against the oldest matching prototype of
    /// `origin` (S4, #1849): the two-layer link between placeholder runs
    /// and the `U=1` registry. First registered match wins
    /// (deterministic); `None` when no prototype of `origin` serves the
    /// run, when the run scrolled fully off the top, or when clipping
    /// leaves nothing painted. `scrollback_now` is the current
    /// `State::scrollback_len()`: callers holding print-time-anchored
    /// runs pass it for the blit-style delta, while live-grid callers
    /// (whose decoded rows are already scrolled truth) pass the
    /// prototype's own base so the delta cancels.
    #[must_use]
    pub fn virtual_run_rect(
        &self,
        origin: Option<u64>,
        run: &KittyUnicodeRun,
        cells: &[KittyUnicodeCell],
        metrics: CellMetrics,
        scrollback_now: usize,
    ) -> Option<KittyUnicodeRect> {
        self.virtual_protos
            .iter()
            .filter(|v| v.origin == origin)
            .find_map(|v| {
                let virt = v.as_unicode_virtual();
                unicode_run_rect(
                    run,
                    cells,
                    &virt,
                    metrics,
                    scrollback_now,
                    v.scrollback_base,
                )
            })
    }

    /// Drops every virtual prototype of `origin`, keeping blit placements
    /// and all stored images (S4, #1849).
    ///
    /// `ED 2`/`ED 22`/`FullReset` counterpart to [`Self::clear_origin`]:
    /// terminal truth drops prototypes on full clear (`clear_screen`) but
    /// spares them on `d=a` (`AllVisible`), so the two clear paths stay
    /// split here too.
    pub fn clear_virtual_for_origin(&mut self, origin: Option<u64>) {
        self.virtual_protos.retain(|v| v.origin != origin);
    }

    /// Whether no placement is retained.
    #[must_use]
    pub fn placement_is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Stores a decoded bitmap, validating decode caps before admission.
    ///
    /// Primary-origin shorthand for
    /// [`KittyImageLayer::store_for_origin`] (`origin == None`); pane
    /// sessions use `store_for_origin` with their origin token so the
    /// bytes count against the emitting origin's quota.
    ///
    /// # Errors
    ///
    /// Same as [`KittyImageLayer::store_for_origin`].
    pub fn store(
        &mut self,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed_len: usize,
    ) -> Result<KittyImageId, KittyPlacementError> {
        self.store_for_origin(width, height, rgba, compressed_len, None)
    }

    /// Stores a decoded bitmap for one origin (S5 per-origin quotas,
    /// #1849), validating decode caps before admission.
    ///
    /// `rgba` must be exactly `width * height * 4` bytes. On success the
    /// oldest images *of this origin* are evicted first (FIFO within the
    /// origin) to satisfy the per-origin count and byte quotas, and
    /// placements of evicted images are dropped deterministically. Other
    /// origins are never evicted: when this origin is within its own quota
    /// but a global bound is already held by other origins, the store is
    /// refused with [`KittyPlacementError::QuotaExceeded`] instead.
    /// Origin count stays bounded by the live pane-session count plus the
    /// primary grid: accounting scans the bounded deques, so no per-origin
    /// table is ever allocated.
    ///
    /// The wire identity defaults to anonymous (`0`); callers with a
    /// protocol image id use
    /// [`KittyImageLayer::store_for_origin_with_wire`] so later `a=f`/`a=c`
    /// commands resolve this image as their animation target.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError`] dimension/area/byte/length rejections, or
    /// [`KittyPlacementError::DecodedTooLarge`] when the bitmap alone
    /// exceeds the layer byte cap, or
    /// [`KittyPlacementError::QuotaExceeded`] on global pressure from other
    /// origins. Failures store nothing and evict nothing.
    pub fn store_for_origin(
        &mut self,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed_len: usize,
        origin: Option<u64>,
    ) -> Result<KittyImageId, KittyPlacementError> {
        self.store_for_origin_with_wire(width, height, rgba, compressed_len, origin, 0)
    }

    /// Stores a decoded bitmap for one origin with wire identity (S1
    /// animation targets, #1849).
    ///
    /// Identical to [`KittyImageLayer::store_for_origin`] except the image
    /// records the wire `i=` id (`0` anonymous): later `a=f`/`a=c`
    /// commands addressing `(origin, wire_image)` resolve to the newest
    /// image stored under that pair (see
    /// [`KittyImageLayer::find_root_for_origin`]).
    ///
    /// # Errors
    ///
    /// Same as [`KittyImageLayer::store_for_origin`].
    pub fn store_for_origin_with_wire(
        &mut self,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        compressed_len: usize,
        origin: Option<u64>,
        wire_image: u32,
    ) -> Result<KittyImageId, KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        let bytes = rgba.len();
        if bytes > KITTY_PLACE_MAX_BYTES {
            return Err(KittyPlacementError::DecodedTooLarge {
                bytes,
                cap: KITTY_PLACE_MAX_BYTES,
            });
        }
        // Per-origin pressure, oldest first (front of the deque): collect
        // the own-origin images that must go so the quotas below hold.
        // Computed before any mutation so a refusal evicts nothing.
        let mut evict: Vec<KittyImageId> = Vec::new();
        let mut freed_bytes = 0usize;
        let mut kept_count = self.image_count_for_origin(origin);
        let mut kept_bytes = self.bytes_for_origin(origin);
        for img in &self.images {
            if kept_count.saturating_add(1) <= KITTY_PER_ORIGIN_MAX_IMAGES
                && kept_bytes.saturating_add(bytes) <= KITTY_PER_ORIGIN_MAX_BYTES
            {
                break;
            }
            if img.origin == origin {
                evict.push(img.id);
                // S1 (#1849): eviction frees the whole animation (root
                // plus every appended frame), never one frame.
                freed_bytes = freed_bytes.saturating_add(img.stored_bytes());
                kept_count = kept_count.saturating_sub(1);
                kept_bytes = kept_bytes.saturating_sub(img.stored_bytes());
            }
        }
        // Global pressure from other origins refuses instead of evicting a
        // victim (no cross-origin eviction).
        let global_len_after = self.images.len().saturating_sub(evict.len());
        let global_bytes_after = self.total_bytes.saturating_sub(freed_bytes);
        if global_len_after.saturating_add(1) > KITTY_PLACE_MAX_IMAGES
            || global_bytes_after.saturating_add(bytes) > KITTY_PLACE_MAX_BYTES
        {
            return Err(KittyPlacementError::QuotaExceeded);
        }
        if !evict.is_empty() {
            self.images.retain(|img| !evict.contains(&img.id));
            self.total_bytes = self.total_bytes.saturating_sub(freed_bytes);
            self.placements.retain(|p| !evict.contains(&p.image));
            // S4 (#1849): combined-form prototypes link their stored
            // image and cascade like blit placements; bodiless
            // prototypes (`None`) have no link and survive.
            self.virtual_protos
                .retain(|v| v.image.is_none_or(|id| !evict.contains(&id)));
        }
        let id = KittyImageId(self.next_image_id);
        self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
        let generation = self.next_image_generation;
        self.next_image_generation = self.next_image_generation.wrapping_add(1).max(1);
        self.images.push_back(KittyPlacedImage {
            id,
            generation,
            origin,
            wire_image,
            width,
            height,
            rgba,
            frames: Vec::new(),
            compressed_len,
        });
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        Ok(id)
    }

    /// Resolves the animation root for one origin-scoped wire identity
    /// (S1, #1849): the newest stored image with `(origin, wire_image)`,
    /// or `None` when no such image is held. `wire_image == 0`
    /// (anonymous) never resolves — anonymous images are not animation
    /// targets, mirroring the terminal-truth store.
    #[must_use]
    pub fn find_root_for_origin(
        &self,
        origin: Option<u64>,
        wire_image: u32,
    ) -> Option<KittyImageId> {
        if wire_image == 0 {
            return None;
        }
        self.images
            .iter()
            .rev()
            .find(|img| img.origin == origin && img.wire_image == wire_image)
            .map(|img| img.id)
    }

    /// Frame count of one stored image, root included (`None` when the
    /// image is unknown or evicted).
    #[must_use]
    pub fn frame_count(&self, id: KittyImageId) -> Option<u32> {
        self.get(id).map(|img| img.frame_count())
    }

    /// Byte view of one stored animation frame (S1, #1849): frame 1 is the
    /// root ([`KittyPlacedImage::rgba`]), frames 2..=N are the appended
    /// [`KittyFrame`] records. Returns the frame dimensions plus the
    /// row-major RGBA8 bytes, or `None` when the image is unknown or the
    /// 1-based frame has no stored pixels (fail closed: the present layer
    /// paints nothing for truth frames past the stored range).
    #[must_use]
    pub fn frame_bytes(&self, id: KittyImageId, frame: u32) -> Option<(u32, u32, &[u8])> {
        let img = self.get(id)?;
        if frame == 1 {
            return Some((img.width, img.height, &img.rgba));
        }
        let extra = img.frames.get(frame.checked_sub(2)? as usize)?;
        Some((extra.width, extra.height, &extra.rgba))
    }

    /// Appends one animation frame to a stored image (`a=f` with `r=0`,
    /// S1, #1849).
    ///
    /// `rgba` must be exactly `width * height * 4` bytes and must match
    /// the root frame's extent (see [`KittyPlacementError`]). Every
    /// shipped per-frame cap applies, plus the per-image
    /// ([`KITTY_ANIM_MAX_FRAMES`], [`KITTY_ANIM_MAX_BYTES_PER_IMAGE`]),
    /// per-origin ([`KITTY_PER_ORIGIN_MAX_BYTES`]), and
    /// global ([`KITTY_PLACE_MAX_BYTES`]) totals — all checked before any
    /// buffer grows. Frame ingest never evicts: refusals store nothing and
    /// evict nothing. Returns the new 1-based frame number.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] for unknown images, the
    /// dimension/area/byte/length rejections for over-cap frames,
    /// [`KittyPlacementError::FrameDimensionsMismatch`] for reshaped
    /// frames, [`KittyPlacementError::TooManyFrames`] past 64 frames,
    /// [`KittyPlacementError::AnimationTooLarge`] past the 64 MiB
    /// per-image total, or [`KittyPlacementError::QuotaExceeded`] when the
    /// origin or global store is already full.
    pub fn append_frame(
        &mut self,
        id: KittyImageId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    ) -> Result<u32, KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        let bytes = rgba.len();
        let slot = self
            .images
            .iter()
            .position(|img| img.id == id)
            .ok_or(KittyPlacementError::ImageNotFound(id))?;
        {
            let root = &self.images[slot];
            if width != root.width || height != root.height {
                return Err(KittyPlacementError::FrameDimensionsMismatch {
                    width,
                    height,
                    root_width: root.width,
                    root_height: root.height,
                });
            }
            let count = root.frames.len().saturating_add(1);
            if count >= KITTY_ANIM_MAX_FRAMES {
                return Err(KittyPlacementError::TooManyFrames {
                    count,
                    cap: KITTY_ANIM_MAX_FRAMES,
                });
            }
            let image_total = root.stored_bytes().saturating_add(bytes);
            if image_total > KITTY_ANIM_MAX_BYTES_PER_IMAGE {
                return Err(KittyPlacementError::AnimationTooLarge {
                    total: image_total,
                    cap: KITTY_ANIM_MAX_BYTES_PER_IMAGE,
                });
            }
        }
        let origin = self.images[slot].origin;
        if self.bytes_for_origin(origin).saturating_add(bytes) > KITTY_PER_ORIGIN_MAX_BYTES {
            return Err(KittyPlacementError::QuotaExceeded);
        }
        if self.total_bytes.saturating_add(bytes) > KITTY_PLACE_MAX_BYTES {
            return Err(KittyPlacementError::QuotaExceeded);
        }
        self.images[slot].frames.push(KittyFrame {
            width,
            height,
            rgba,
        });
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        Ok(self.images[slot].frame_count())
    }

    /// Composes a rect from one stored frame onto another in place (`a=c`,
    /// S1, #1849).
    ///
    /// `dest`/`src` are 1-based frame numbers (1 = root);
    /// `(src_x, src_y)` is the wire `X=`/`Y=` source offset,
    /// `(dst_x, dst_y)` the wire `x=`/`y=` destination offset, and
    /// `(w, h)` the wire `w=`/`h=` rect size shared by both rectangles
    /// (`0` defaults to the full frame width/height, per the kitty
    /// graphics protocol). Both frames share the root extent (enforced at
    /// ingest); `mode` is the wire `C=` composition mode,
    /// [`KITTY_COMPOSE_BLEND`] (src-over) or [`KITTY_COMPOSE_REPLACE`]
    /// (byte copy). Each rect must fit the frame bounds with checked `u64`
    /// arithmetic — out-of-bounds, empty, or overflowing rects refuse with
    /// [`KittyPlacementError::ComposeOutOfBounds`] and compose nothing
    /// (the reported rect is the offending one).
    /// A successful compose bumps the image generation (the raster-cache
    /// key binds it; callers additionally drop cached blits wholesale).
    /// `total_bytes` is unchanged (same extent, same length).
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] for unknown images,
    /// [`KittyPlacementError::NoSuchFrame`] for missing frames,
    /// [`KittyPlacementError::UnknownComposeMode`] for modes past 1, or
    /// [`KittyPlacementError::ComposeOutOfBounds`] for unplaceable rects, or
    /// [`KittyPlacementError::ComposeSelfOverlap`] when `dest == src` and
    /// the rectangles overlap (kitty `graphics.c:2077-2083`).
    /// Failures compose nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn compose_frame(
        &mut self,
        id: KittyImageId,
        dest: u32,
        src: u32,
        src_x: u32,
        src_y: u32,
        dst_x: u32,
        dst_y: u32,
        w: u32,
        h: u32,
        mode: u32,
    ) -> Result<(), KittyPlacementError> {
        if mode != KITTY_COMPOSE_BLEND && mode != KITTY_COMPOSE_REPLACE {
            return Err(KittyPlacementError::UnknownComposeMode(mode));
        }
        let slot = self
            .images
            .iter()
            .position(|img| img.id == id)
            .ok_or(KittyPlacementError::ImageNotFound(id))?;
        let count = self.images[slot].frame_count();
        if dest == 0 || dest > count {
            return Err(KittyPlacementError::NoSuchFrame {
                image: id,
                frame: dest,
            });
        }
        if src == 0 || src > count {
            return Err(KittyPlacementError::NoSuchFrame {
                image: id,
                frame: src,
            });
        }
        let (width, height) = {
            let root = &self.images[slot];
            (root.width, root.height)
        };
        // Wire `w=`/`h=` default to the full frame extent; explicit sizes
        // stay as sent.
        let w = if w == 0 { width } else { w };
        let h = if h == 0 { height } else { h };
        // Checked rect fit in `u64`: `u32` ends can never overflow there,
        // and unplaceable (empty or over-edge) rects refuse. Offsets at or
        // past the edge always fail through the end checks below. Each
        // rectangle is checked against the shared frame bounds
        // independently (kitty protocol: `X=`/`Y=` source, `x=`/`y=`
        // destination).
        let src_end_x = u64::from(src_x).saturating_add(u64::from(w));
        let src_end_y = u64::from(src_y).saturating_add(u64::from(h));
        if w == 0 || h == 0 || src_end_x > u64::from(width) || src_end_y > u64::from(height) {
            return Err(KittyPlacementError::ComposeOutOfBounds {
                x: src_x,
                y: src_y,
                w,
                h,
                width,
                height,
            });
        }
        let dst_end_x = u64::from(dst_x).saturating_add(u64::from(w));
        let dst_end_y = u64::from(dst_y).saturating_add(u64::from(h));
        if dst_end_x > u64::from(width) || dst_end_y > u64::from(height) {
            return Err(KittyPlacementError::ComposeOutOfBounds {
                x: dst_x,
                y: dst_y,
                w,
                h,
                width,
                height,
            });
        }
        // Overlapping self-compose (CTX-1124, kitty
        // `graphics.c:2077-2083`): same frame with intersecting rects
        // refuses with `EINVAL`. `MAX(src,dst) < MIN(src,dst)+size` on
        // both axes, so identical and partially overlapping rects refuse
        // while disjoint (including edge-touching) still composes.
        // Checked after the bounds above (kitty order) on the defaulted
        // `w`/`h`, before any copy: failures compose nothing.
        if dest == src {
            let x_overlap = u64::from(src_x.max(dst_x))
                < u64::from(src_x.min(dst_x)).saturating_add(u64::from(w));
            let y_overlap = u64::from(src_y.max(dst_y))
                < u64::from(src_y.min(dst_y)).saturating_add(u64::from(h));
            if x_overlap && y_overlap {
                return Err(KittyPlacementError::ComposeSelfOverlap {
                    src_x,
                    src_y,
                    dst_x,
                    dst_y,
                    w,
                    h,
                });
            }
        }
        // Byte length of the rect window, checked before any copy: bounded
        // by the frame extent (itself within the 64 MiB cap), so the temp
        // source window below can never exceed it.
        let stride = u64::from(width) * 4;
        (u64::from(w))
            .checked_mul(u64::from(h))
            .and_then(|n| n.checked_mul(4))
            .filter(|&n| n <= KITTY_DECODE_MAX_BYTES as u64)
            .ok_or(KittyPlacementError::ComposeOutOfBounds {
                x: src_x,
                y: src_y,
                w,
                h,
                width,
                height,
            })?;
        // Extra-frame slot for 1-based frame numbers past the root.
        let extra_slot = |frame: u32| (frame as usize).saturating_sub(2);
        // Snapshot the source window first so disjoint `src == dest`
        // (non-overlapping self-compose, still allowed) reads stable
        // bytes: the window is at most one frame (64 MiB), admitted
        // above.
        let window: Vec<u8> = {
            let img = &self.images[slot];
            let src_bytes: &[u8] = if src == 1 {
                &img.rgba
            } else {
                &img.frames[extra_slot(src)].rgba
            };
            let row_bytes = (w as usize).saturating_mul(4);
            let mut window = Vec::with_capacity(row_bytes.saturating_mul(h as usize));
            for row in 0..h {
                let start =
                    ((u64::from(src_y) + u64::from(row)) * stride + u64::from(src_x) * 4) as usize;
                let end = start.saturating_add(row_bytes);
                window.extend_from_slice(&src_bytes[start..end]);
            }
            window
        };
        {
            let img = &mut self.images[slot];
            let dest_bytes: &mut [u8] = if dest == 1 {
                &mut img.rgba
            } else {
                &mut img.frames[extra_slot(dest)].rgba
            };
            if mode == KITTY_COMPOSE_REPLACE {
                let row_bytes = (w as usize).saturating_mul(4);
                for row in 0..h {
                    let start = ((u64::from(dst_y) + u64::from(row)) * stride
                        + u64::from(dst_x) * 4) as usize;
                    let end = start.saturating_add(row_bytes);
                    let src_start = (row as usize).saturating_mul(row_bytes);
                    let src_end = src_start.saturating_add(row_bytes);
                    dest_bytes[start..end].copy_from_slice(&window[src_start..src_end]);
                }
            } else {
                blend_rect_over(dest_bytes, &window, width, dst_x, dst_y, w, h);
            }
            img.generation = img.generation.wrapping_add(1).max(1);
        }
        Ok(())
    }

    /// Edits one stored frame's rect in place from decoded wire bytes
    /// (`a=f` with `r>0`, S1, #1849).
    ///
    /// The kitty graphics protocol edits an existing frame by compositing
    /// the transmitted extent onto the frame itself: the background canvas
    /// is the edited frame (`frame`), never the wire `c=` base or `Y=`
    /// color (both are ignored here; the runtime does not forward them).
    /// `(x, y)` is the wire `x=`/`y=` destination offset on the frame and
    /// `width`/`height`/`rgba` the decoded `s=`/`v=` extent; `mode` is the
    /// wire `X=` composition mode ([`KITTY_COMPOSE_BLEND`] src-over or
    /// [`KITTY_COMPOSE_REPLACE`] byte copy). The rect must fit the shared
    /// root extent with checked `u64` arithmetic — out-of-bounds, empty, or
    /// overflowing rects refuse with
    /// [`KittyPlacementError::ComposeOutOfBounds`] and edit nothing (the
    /// reported rect is the offending one). The decoded bytes reuse the
    /// shipped per-frame caps through `checked_bitmap`, so over-cap
    /// extents refuse before any copy.
    /// A successful edit bumps the image generation (the raster-cache key
    /// binds it; callers additionally drop cached blits wholesale).
    /// `total_bytes` is unchanged (same extent, same length), so the
    /// per-image ([`KITTY_ANIM_MAX_BYTES_PER_IMAGE`]), per-origin
    /// ([`KITTY_PER_ORIGIN_MAX_BYTES`]), and global
    /// ([`KITTY_PLACE_MAX_BYTES`]) totals cannot trip here: an edit grows
    /// no buffer and evicts nothing.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] for unknown images,
    /// [`KittyPlacementError::NoSuchFrame`] for a missing frame,
    /// [`KittyPlacementError::UnknownComposeMode`] for modes past 1, the
    /// `checked_bitmap` dimension/area/byte/length rejections for
    /// over-cap extents, or [`KittyPlacementError::ComposeOutOfBounds`]
    /// for unplaceable rects. Failures edit nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn edit_frame(
        &mut self,
        id: KittyImageId,
        frame: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
        mode: u32,
    ) -> Result<(), KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        if mode != KITTY_COMPOSE_BLEND && mode != KITTY_COMPOSE_REPLACE {
            return Err(KittyPlacementError::UnknownComposeMode(mode));
        }
        let slot = self
            .images
            .iter()
            .position(|img| img.id == id)
            .ok_or(KittyPlacementError::ImageNotFound(id))?;
        let count = self.images[slot].frame_count();
        if frame == 0 || frame > count {
            return Err(KittyPlacementError::NoSuchFrame { image: id, frame });
        }
        let (root_width, root_height) = {
            let root = &self.images[slot];
            (root.width, root.height)
        };
        // Checked rect fit in `u64`: the decoded extent must land inside
        // the shared frame bounds (`u32` ends can never overflow there).
        // `checked_bitmap` above already refused empty extents through the
        // exact-length check, so a zero `w`/`h` here is defense-in-depth.
        let end_x = u64::from(x).saturating_add(u64::from(width));
        let end_y = u64::from(y).saturating_add(u64::from(height));
        if width == 0
            || height == 0
            || end_x > u64::from(root_width)
            || end_y > u64::from(root_height)
        {
            return Err(KittyPlacementError::ComposeOutOfBounds {
                x,
                y,
                w: width,
                h: height,
                width: root_width,
                height: root_height,
            });
        }
        // Extra-frame slot for 1-based frame numbers past the root.
        let extra_slot = (frame as usize).saturating_sub(2);
        {
            let img = &mut self.images[slot];
            let dest_bytes: &mut [u8] = if frame == 1 {
                &mut img.rgba
            } else {
                &mut img.frames[extra_slot].rgba
            };
            if mode == KITTY_COMPOSE_REPLACE {
                let row_bytes = (width as usize).saturating_mul(4);
                for row in 0..height {
                    let start = (((u64::from(y) + u64::from(row)) * u64::from(root_width)
                        + u64::from(x)) as usize)
                        .saturating_mul(4);
                    let end = start.saturating_add(row_bytes);
                    let src_start = (row as usize).saturating_mul(row_bytes);
                    let src_end = src_start.saturating_add(row_bytes);
                    dest_bytes[start..end].copy_from_slice(&rgba[src_start..src_end]);
                }
            } else {
                blend_rect_over(dest_bytes, rgba, root_width, x, y, width, height);
            }
            img.generation = img.generation.wrapping_add(1).max(1);
        }
        Ok(())
    }

    /// Builds one full-extent animation frame from partial wire data and
    /// appends it (`a=f` partial ingest, S1, #1849).
    ///
    /// The kitty graphics protocol sends partial frames as a small
    /// `width` x `height` payload at wire offset (`x=`, `y=`) composited
    /// onto a background canvas: wire `c=` base frame `base_frame` (0 for
    /// none), else the wire `Y=` 32-bit RGBA `background` color (0 for
    /// transparent black), with wire `X=` composition `mode`
    /// ([`KITTY_COMPOSE_BLEND`] src-over or [`KITTY_COMPOSE_REPLACE`]
    /// byte copy). The composed full-extent frame then ingests through
    /// [`Self::append_frame`], so every per-image/per-origin/global quota
    /// still binds and ingest still never evicts.
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] for unknown images,
    /// [`KittyPlacementError::UnknownComposeMode`] for modes past 1,
    /// [`KittyPlacementError::NoSuchFrame`] for a missing base frame,
    /// [`KittyPlacementError::ComposeOutOfBounds`] when the update rect
    /// leaves the root extent, else the [`Self::append_frame`] taxonomy.
    /// Failures store nothing and evict nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn append_frame_composited(
        &mut self,
        id: KittyImageId,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        base_frame: u32,
        background: u32,
        mode: u32,
    ) -> Result<u32, KittyPlacementError> {
        checked_bitmap(width, height, rgba.len())?;
        if mode != KITTY_COMPOSE_BLEND && mode != KITTY_COMPOSE_REPLACE {
            return Err(KittyPlacementError::UnknownComposeMode(mode));
        }
        let slot = self
            .images
            .iter()
            .position(|img| img.id == id)
            .ok_or(KittyPlacementError::ImageNotFound(id))?;
        let (root_width, root_height) = {
            let root = &self.images[slot];
            (root.width, root.height)
        };
        let end_x = u64::from(x).saturating_add(u64::from(width));
        let end_y = u64::from(y).saturating_add(u64::from(height));
        if width == 0
            || height == 0
            || end_x > u64::from(root_width)
            || end_y > u64::from(root_height)
        {
            return Err(KittyPlacementError::ComposeOutOfBounds {
                x,
                y,
                w: width,
                h: height,
                width: root_width,
                height: root_height,
            });
        }
        // Background canvas: the base frame's bytes, else the `Y=` color
        // fill (32-bit RGBA, big-endian byte order: `0xff0000ff` is opaque
        // red), else transparent black.
        let mut canvas: Vec<u8> = if base_frame != 0 {
            let img = &self.images[slot];
            let count = img.frame_count();
            if base_frame > count {
                return Err(KittyPlacementError::NoSuchFrame {
                    image: id,
                    frame: base_frame,
                });
            }
            let base: &[u8] = if base_frame == 1 {
                &img.rgba
            } else {
                &img.frames[(base_frame as usize).saturating_sub(2)].rgba
            };
            base.to_vec()
        } else {
            background
                .to_be_bytes()
                .repeat((root_width as usize).saturating_mul(root_height as usize))
        };
        if mode == KITTY_COMPOSE_REPLACE {
            let stride = (root_width as usize).saturating_mul(4);
            let row_bytes = (width as usize).saturating_mul(4);
            for row in 0..height {
                let dest = ((y as usize).saturating_add(row as usize)).saturating_mul(stride)
                    + (x as usize).saturating_mul(4);
                let src = (row as usize).saturating_mul(row_bytes);
                canvas[dest..dest.saturating_add(row_bytes)]
                    .copy_from_slice(&rgba[src..src.saturating_add(row_bytes)]);
            }
        } else {
            blend_rect_over(&mut canvas, &rgba, root_width, x, y, width, height);
        }
        self.append_frame(id, root_width, root_height, canvas)
    }

    /// Looks up a stored image by id.
    #[must_use]
    pub fn get(&self, id: KittyImageId) -> Option<&KittyPlacedImage> {
        self.images.iter().find(|img| img.id == id)
    }

    /// Tags a stored image with its wire `i=` id (S1 animation targets,
    /// #1849). Anonymous (`0`) is a no-op; any previous tag is
    /// overwritten (a re-place of one stored image under a new wire id
    /// supersedes it — placements keep their own per-placement wire
    /// identity regardless). No-op for unknown images.
    pub fn set_wire_image(&mut self, id: KittyImageId, wire_image: u32) {
        if wire_image == 0 {
            return;
        }
        if let Some(slot) = self.images.iter_mut().find(|img| img.id == id) {
            slot.wire_image = wire_image;
        }
    }

    /// Removes the image with `id` and its placements; `true` when removed.
    ///
    /// S1 (#1849): frees the whole animation (root plus every appended
    /// frame), never one frame.
    pub fn remove(&mut self, id: KittyImageId) -> bool {
        let before = self.images.len();
        let mut removed_bytes = 0usize;
        self.images.retain(|img| {
            if img.id == id {
                removed_bytes = removed_bytes.saturating_add(img.stored_bytes());
                false
            } else {
                true
            }
        });
        let removed = self.images.len() != before;
        if removed {
            self.total_bytes = self.total_bytes.saturating_sub(removed_bytes);
            self.placements.retain(|p| p.image != id);
            // Issue #2875: dropping an image drops its placements, so their
            // relative children orphan and follow (truth cascades too).
            self.cascade_relative_orphans();
            // S4 (#1849): linked combined-form prototypes cascade with
            // their image, like blit placements.
            self.virtual_protos.retain(|v| v.image != Some(id));
        }
        removed
    }

    /// Removes every stored image, placement, and virtual prototype matching
    /// `(origin, wire_image)` unless the stored image is in `protected_images`
    /// (issue #3159).
    ///
    /// Used by the runtime when a combined `a=T,U=1,i=<id>` re-transmits an
    /// explicit non-zero wire image id on `origin`, so repeated inline-image
    /// or animated-mascot updates replace the previous frame in place instead
    /// of leaking images and virtual prototypes until per-origin or global
    /// quotas lock out the pane. Images referenced by `kitty_alt_stash`
    /// (`protected_images`) stay stored so leaving the alternate screen can
    /// restore the main plane intact. Anonymous (`wire_image == 0`) is a no-op.
    pub fn remove_by_wire_image(
        &mut self,
        origin: Option<u64>,
        wire_image: u32,
        protected_images: &[KittyImageId],
    ) -> bool {
        if wire_image == 0 {
            return false;
        }
        let dead: Vec<KittyImageId> = self
            .images
            .iter()
            .filter(|img| {
                img.origin == origin
                    && img.wire_image == wire_image
                    && !protected_images.contains(&img.id)
            })
            .map(|img| img.id)
            .collect();
        let before_images = self.images.len();
        let before_items = self.placements.len() + self.virtual_protos.len();
        if !dead.is_empty() {
            let mut freed_bytes = 0usize;
            self.images.retain(|img| {
                if dead.contains(&img.id) {
                    freed_bytes = freed_bytes.saturating_add(img.stored_bytes());
                    false
                } else {
                    true
                }
            });
            self.total_bytes = self.total_bytes.saturating_sub(freed_bytes);
        }
        self.placements.retain(|p| {
            !(p.origin == origin && p.wire_image == wire_image) && !dead.contains(&p.image)
        });
        self.virtual_protos.retain(|v| {
            !(v.origin == origin && v.wire_image == wire_image)
                && v.image.is_none_or(|id| !dead.contains(&id))
        });
        self.cascade_relative_orphans();
        self.images.len() != before_images
            || (self.placements.len() + self.virtual_protos.len()) != before_items
    }

    /// Drops older stored images for `(origin, wire_image)` other than `keep`
    /// that have no remaining blit placement, linked virtual prototype, or
    /// stashed alternate-screen reference (`protected_images`) (issue #3159).
    pub fn prune_superseded_wire_images(
        &mut self,
        origin: Option<u64>,
        wire_image: u32,
        keep: KittyImageId,
        protected_images: &[KittyImageId],
    ) -> bool {
        if wire_image == 0 {
            return false;
        }
        let dead: Vec<KittyImageId> = self
            .images
            .iter()
            .filter(|img| {
                img.origin == origin
                    && img.wire_image == wire_image
                    && img.id != keep
                    && !protected_images.contains(&img.id)
                    && !self.placements.iter().any(|p| p.image == img.id)
                    && !self.virtual_protos.iter().any(|v| v.image == Some(img.id))
            })
            .map(|img| img.id)
            .collect();
        if dead.is_empty() {
            return false;
        }
        let mut freed_bytes = 0usize;
        self.images.retain(|img| {
            if dead.contains(&img.id) {
                freed_bytes = freed_bytes.saturating_add(img.stored_bytes());
                false
            } else {
                true
            }
        });
        self.total_bytes = self.total_bytes.saturating_sub(freed_bytes);
        true
    }

    /// Whether an addressed placement (`wire_image != 0 && wire_placement != 0`)
    /// already exists on `origin` and will be replaced in place by
    /// `push_placement` without consuming quota (issue #3159).
    #[must_use]
    pub fn has_addressed_placement(
        &self,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
    ) -> bool {
        wire_image != 0
            && wire_placement != 0
            && self.placements.iter().any(|p| {
                p.origin == origin
                    && p.wire_image == wire_image
                    && p.wire_placement == wire_placement
            })
    }

    /// Whether an addressed virtual prototype (`wire_image != 0 &&
    /// wire_placement != 0`) already exists on `origin` and will be replaced
    /// in place by [`Self::register_virtual_for_origin`] without consuming
    /// quota (issue #3159).
    #[must_use]
    pub fn has_addressed_virtual(
        &self,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
    ) -> bool {
        wire_image != 0
            && wire_placement != 0
            && self.virtual_protos.iter().any(|v| {
                v.origin == origin
                    && v.wire_image == wire_image
                    && v.wire_placement == wire_placement
            })
    }

    /// Reclaims store capacity for a runtime transmit when global image or
    /// byte bounds would otherwise refuse after own-origin FIFO eviction
    /// (issue #3159).
    ///
    /// Long-running multi-pane sessions can collectively reach
    /// [`KITTY_PLACE_MAX_IMAGES`] (64) or [`KITTY_PLACE_MAX_BYTES`] (320 MiB)
    /// while no single origin has reached its 64-image per-origin FIFO
    /// threshold. Reclaiming the oldest unreferenced image first, then the
    /// transmitting origin's oldest image, and finally the oldest global
    /// image keeps active panes functional without raising memory bounds.
    pub fn reclaim_for_runtime_store(
        &mut self,
        origin: Option<u64>,
        incoming_bytes: usize,
    ) -> bool {
        if incoming_bytes > KITTY_PLACE_MAX_BYTES {
            return false;
        }
        let mut sim_evict_count = 0usize;
        let mut sim_freed_bytes = 0usize;
        let mut kept_count = self.image_count_for_origin(origin);
        let mut kept_bytes = self.bytes_for_origin(origin);
        for img in &self.images {
            if kept_count.saturating_add(1) <= KITTY_PER_ORIGIN_MAX_IMAGES
                && kept_bytes.saturating_add(incoming_bytes) <= KITTY_PER_ORIGIN_MAX_BYTES
            {
                break;
            }
            if img.origin == origin {
                sim_evict_count = sim_evict_count.saturating_add(1);
                sim_freed_bytes = sim_freed_bytes.saturating_add(img.stored_bytes());
                kept_count = kept_count.saturating_sub(1);
                kept_bytes = kept_bytes.saturating_sub(img.stored_bytes());
            }
        }
        let global_len_after = self.images.len().saturating_sub(sim_evict_count);
        let global_bytes_after = self.total_bytes.saturating_sub(sim_freed_bytes);
        if global_len_after.saturating_add(1) <= KITTY_PLACE_MAX_IMAGES
            && global_bytes_after.saturating_add(incoming_bytes) <= KITTY_PLACE_MAX_BYTES
        {
            return false;
        }
        let mut evicted = false;
        while self.images.len().saturating_add(1) > KITTY_PLACE_MAX_IMAGES
            || self.total_bytes.saturating_add(incoming_bytes) > KITTY_PLACE_MAX_BYTES
        {
            let candidate = self
                .images
                .iter()
                .find(|img| {
                    !self.placements.iter().any(|p| p.image == img.id)
                        && !self.virtual_protos.iter().any(|v| v.image == Some(img.id))
                })
                .or_else(|| self.images.iter().find(|img| img.origin == origin))
                .or_else(|| self.images.front())
                .map(|img| img.id);
            let Some(id) = candidate else {
                break;
            };
            if !self.remove(id) {
                break;
            }
            evicted = true;
        }
        evicted
    }

    /// Reclaims placement/prototype budget for a runtime display or virtual
    /// registration when cross-kind per-origin sharing or multi-origin global
    /// saturation would otherwise refuse (issue #3159).
    pub fn reclaim_for_runtime_placement(
        &mut self,
        origin: Option<u64>,
        for_virtual: bool,
    ) -> bool {
        let mut evicted = false;
        let own_placements = self.placement_count_for_origin(origin);
        let own_virtuals = self.virtual_count_for_origin(origin);
        let own_total = own_placements + own_virtuals;
        if own_total >= KITTY_PER_ORIGIN_MAX_PLACEMENTS {
            if !for_virtual && own_placements == 0 {
                if let Some(pos) = self.virtual_protos.iter().position(|v| v.origin == origin) {
                    self.virtual_protos.remove(pos);
                    evicted = true;
                }
            } else if for_virtual && own_virtuals == 0 {
                if let Some(pos) = self.placements.iter().position(|p| p.origin == origin) {
                    self.placements.remove(pos);
                    self.cascade_relative_orphans();
                    evicted = true;
                }
            }
        }
        loop {
            let own_placements = self.placement_count_for_origin(origin);
            let own_virtuals = self.virtual_count_for_origin(origin);
            let own_total = own_placements + own_virtuals;
            let own_evictable = if for_virtual {
                own_virtuals
            } else {
                own_placements
            };
            let evict_own = own_total >= KITTY_PER_ORIGIN_MAX_PLACEMENTS && own_evictable > 0;
            let global_total = self.placements.len() + self.virtual_protos.len();
            let global_len_after = global_total.saturating_sub(usize::from(evict_own));
            let virtual_cap_exceeded = for_virtual
                && self.virtual_protos.len() >= KITTY_VIRTUAL_MAX_ITEMS
                && own_virtuals == 0;
            if global_len_after < KITTY_PLACE_MAX_ITEMS && !virtual_cap_exceeded {
                break;
            }
            if virtual_cap_exceeded {
                if self.virtual_protos.pop_front().is_some() {
                    evicted = true;
                    continue;
                }
                break;
            }
            if let Some(pos) = self.virtual_protos.iter().position(|v| v.origin == origin) {
                self.virtual_protos.remove(pos);
                evicted = true;
            } else if let Some(pos) = self.placements.iter().position(|p| p.origin == origin) {
                self.placements.remove(pos);
                self.cascade_relative_orphans();
                evicted = true;
            } else if !self.virtual_protos.is_empty() {
                self.virtual_protos.pop_front();
                evicted = true;
            } else if !self.placements.is_empty() {
                self.placements.pop_front();
                self.cascade_relative_orphans();
                evicted = true;
            } else {
                break;
            }
        }
        evicted
    }

    /// Places a stored image at the cursor cell.
    ///
    /// `cols`/`rows` are the explicit `c=`/`r=` spans (0 or absent derives
    /// from decoded pixels). `scrollback_base` is
    /// `State::scrollback_len()` now. Evicts the origin's oldest placement
    /// at the per-origin quota (FIFO within the origin; S5, #1849) and
    /// refuses with [`KittyPlacementError::QuotaExceeded`] on global
    /// pressure from other origins. The placement is bound to the primary
    /// origin (`None`); pane sessions use
    /// [`KittyImageLayer::display_for_origin`].
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
    /// and per-origin eviction behavior as
    /// [`KittyImageLayer::display_for_origin_with_wire`]. The wire
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
    /// S5 per-origin quotas (#1849): the placement counts against `origin`'s
    /// 32-placement quota and evicts that origin's own oldest placement
    /// first (FIFO within the origin). Other origins are never evicted:
    /// when `origin` is within its own quota but the 128 global bound is
    /// already held by other origins, the placement is refused with
    /// [`KittyPlacementError::QuotaExceeded`] instead. Dangling references
    /// (image evicted under the store quotas) still fail closed with
    /// [`KittyPlacementError::ImageNotFound`].
    ///
    /// # Errors
    ///
    /// [`KittyPlacementError::ImageNotFound`] when `image` is unknown, or
    /// [`KittyPlacementError::QuotaExceeded`] on global pressure from other
    /// origins. Failures store nothing new and evict nothing.
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
        self.push_placement(
            image,
            Some((anchor_col, anchor_row)),
            cols,
            rows,
            metrics,
            scrollback_base,
            z,
            origin,
            wire_image,
            wire_placement,
            None,
            (0, 0),
        )
    }

    /// Places a stored image relative to another placement (issue #2875).
    ///
    /// The wire form is `a=p` with `P=`/`Q=` (parent image / placement ids,
    /// `Q=0` = any placement of that image) plus `H=`/`V=` cell offsets. The
    /// parent link is recorded verbatim — never a resolved anchor — so the
    /// paint path derives the child rect from wherever the parent paints
    /// *this* frame ([`Self::placement_full_rect_resolved`]): a scrolling or
    /// clipped parent moves or hides the child with no extra bookkeeping,
    /// and a parent that disappears paints the child nowhere (fail closed).
    ///
    /// Terminal truth (`bitty_term_state::PlacementStore`) already refused a
    /// missing or virtual parent before this admission; presentation still
    /// re-resolves the whole chain at paint time, so an unreachable link, a
    /// cycle, or an offset the pixel range cannot hold paints nothing.
    ///
    /// Spans, `z`, quotas, FIFO eviction, and the origin-scoped wire
    /// identity are exactly [`Self::display_for_origin_with_wire`]'s.
    ///
    /// # Errors
    ///
    /// Same as [`Self::display_for_origin_with_wire`].
    #[allow(clippy::too_many_arguments)]
    pub fn display_relative_for_origin(
        &mut self,
        image: KittyImageId,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
        parent: (u32, u32),
        parent_offset: (i32, i32),
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        self.push_placement(
            image,
            None,
            cols,
            rows,
            metrics,
            scrollback_base,
            z,
            origin,
            wire_image,
            wire_placement,
            Some(parent),
            parent_offset,
        )
    }

    /// Shared admission behind the absolute and relative placement seams.
    ///
    /// `anchor` is `None` for a relative placement (its rect comes from the
    /// parent at paint time); the stored cell is `(0, 0)` and unused.
    #[allow(clippy::too_many_arguments)]
    fn push_placement(
        &mut self,
        image: KittyImageId,
        anchor: Option<(u16, u16)>,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        scrollback_base: usize,
        z: i32,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
        parent: Option<(u32, u32)>,
        parent_offset: (i32, i32),
    ) -> Result<KittyPlacementId, KittyPlacementError> {
        let stored = self
            .get(image)
            .ok_or(KittyPlacementError::ImageNotFound(image))?;
        let cols = cell_span(cols, stored.width, metrics.width);
        let rows = cell_span(rows, stored.height, metrics.height);
        let (anchor_col, anchor_row) = anchor.unwrap_or((0, 0));
        let placement = KittyPlacement {
            id: KittyPlacementId(0),
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
            parent,
            parent_offset,
        };
        // An addressed re-put (`i != 0` and `p != 0`) replaces in place,
        // exactly like terminal truth's upsert: no quota consumed, nothing
        // evicted, and — load-bearing for relative placements (#2875) — no
        // cascade, so children pointing at this wire identity keep
        // resolving. Replacing by delete-then-add would drop them.
        if wire_image != 0 && wire_placement != 0 {
            if let Some(slot) = self.placements.iter_mut().find(|p| {
                p.origin == origin
                    && p.wire_image == wire_image
                    && p.wire_placement == wire_placement
            }) {
                let id = slot.id;
                *slot = KittyPlacement { id, ..placement };
                return Ok(id);
            }
        }
        // Per-origin pressure evicts this origin's own oldest placement;
        // global pressure from other origins refuses instead (no
        // cross-origin eviction). Decided before any mutation so a refusal
        // evicts nothing (shared with `check_placement_quota_for_origin`,
        // which transmit-and-display preflights before storing). S4
        // (#1849): prototypes share the budget, so the counts below cover
        // both deques with same-kind eviction only.
        self.check_placement_quota_for_origin(origin)?;
        let evict_own = self.placement_count_for_origin(origin)
            + self.virtual_count_for_origin(origin)
            >= KITTY_PER_ORIGIN_MAX_PLACEMENTS
            && self.placement_count_for_origin(origin) > 0;
        if evict_own {
            if let Some(pos) = self.placements.iter().position(|p| p.origin == origin) {
                self.placements.remove(pos);
            }
        }
        let id = KittyPlacementId(self.next_placement_id);
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        self.placements
            .push_back(KittyPlacement { id, ..placement });
        Ok(id)
    }

    /// Looks up a placement by id.
    #[must_use]
    pub fn get_placement(&self, id: KittyPlacementId) -> Option<&KittyPlacement> {
        self.placements.iter().find(|p| p.id == id)
    }

    /// Removes a placement by id, cascading to its relative children
    /// (issue #2875: presentation mirrors terminal truth's cascade).
    pub fn remove_placement(&mut self, id: KittyPlacementId) -> bool {
        let before = self.placements.len();
        self.placements.retain(|p| p.id != id);
        let removed = self.placements.len() != before;
        if removed {
            self.cascade_relative_orphans();
        }
        removed
    }

    /// Iterates placements oldest first.
    pub fn placements(&self) -> impl Iterator<Item = &KittyPlacement> {
        self.placements.iter()
    }

    /// Detaches every placement of `origin`, preserving their relative
    /// order (the alternate-screen stash half, #2442).
    ///
    /// The caller owns the returned entries and may re-attach them with
    /// [`Self::restore_placements`]; images, ids, and next-id counters are
    /// untouched, so a later main-screen image never collides with a
    /// stashed one. Bounded by the placement caps.
    pub fn take_placements_for(&mut self, origin: Option<u64>) -> Vec<KittyPlacement> {
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(self.placements.len());
        while let Some(placement) = self.placements.pop_front() {
            if placement.origin == origin {
                taken.push(placement);
            } else {
                kept.push_back(placement);
            }
        }
        self.placements = kept;
        taken
    }

    /// Re-attaches placements previously detached by
    /// [`Self::take_placements_for`], preserving their order.
    pub fn restore_placements(&mut self, placements: Vec<KittyPlacement>) {
        self.placements.extend(placements);
    }

    /// Detaches every virtual prototype of `origin` (alternate-screen
    /// stash half; placement counterpart is
    /// [`Self::take_placements_for`]).
    pub fn take_virtuals_for(&mut self, origin: Option<u64>) -> Vec<KittyVirtualPrototype> {
        let mut taken = Vec::new();
        let mut kept = VecDeque::with_capacity(self.virtual_protos.len());
        while let Some(proto) = self.virtual_protos.pop_front() {
            if proto.origin == origin {
                taken.push(proto);
            } else {
                kept.push_back(proto);
            }
        }
        self.virtual_protos = kept;
        taken
    }

    /// Re-attaches virtual prototypes previously detached by
    /// [`Self::take_virtuals_for`], preserving their order.
    pub fn restore_virtuals(&mut self, protos: Vec<KittyVirtualPrototype>) {
        self.virtual_protos.extend(protos);
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
    /// Alternate-screen plane swaps call this for the swapped origin only:
    /// entering alt starts the fresh alt plane empty, and leaving alt
    /// clears the alt plane before main is re-attached, so one pane's
    /// fullscreen app never wipes another pane's images. Images left
    /// without placements stay inert (never painted) and age out under the
    /// store caps.
    pub fn clear_origin(&mut self, origin: Option<u64>) {
        self.placements.retain(|p| p.origin != origin);
    }

    /// Retires `origin`: drops its stored images plus every placement of
    /// that origin and every placement bound to a removed image, keeping
    /// all other origins intact (S5 per-origin quotas, #1849).
    ///
    /// Pane-teardown counterpart to [`KittyImageLayer::clear_origin`]:
    /// a closed or respawned pane's origin token is dead, so its decoded
    /// images must not keep occupying the shared image/byte caps after
    /// the pane is gone (S5 refuses cross-origin eviction, so leaked
    /// slots would block other origins with
    /// [`KittyPlacementError::QuotaExceeded`]). Alternate-screen plane
    /// swaps, protocol deletion, and display-erase clearing keep using
    /// `clear_origin` (placements drop, images stay inert under the
    /// store caps).
    pub fn retire_origin(&mut self, origin: Option<u64>) {
        // Bounded scans (at most `KITTY_PLACE_MAX_IMAGES` images,
        // `KITTY_PLACE_MAX_ITEMS` placements), same style as the
        // store-path FIFO collection.
        let dead: Vec<KittyImageId> = self
            .images
            .iter()
            .filter(|img| img.origin == origin)
            .map(|img| img.id)
            .collect();
        if !dead.is_empty() {
            let mut freed_bytes = 0usize;
            self.images.retain(|img| {
                if img.origin == origin {
                    // S1 (#1849): frees whole animations (root plus
                    // frames), like `remove`.
                    freed_bytes = freed_bytes.saturating_add(img.stored_bytes());
                    false
                } else {
                    true
                }
            });
            self.total_bytes = self.total_bytes.saturating_sub(freed_bytes);
        }
        self.placements
            .retain(|p| p.origin != origin && !dead.contains(&p.image));
        // S4 (#1849): a retired origin's prototypes die with it, like its
        // placements (terminal truth drops the whole pane `State`).
        self.virtual_protos.retain(|v| v.origin != origin);
    }

    /// Placement-quota preflight for one origin: the refusal half of
    /// [`KittyImageLayer::display_for_origin_with_wire`] without mutation.
    ///
    /// Transmit-and-display calls this before storing so a placement-quota
    /// refusal leaves no new image and never discards FIFO-evicted images
    /// (S5, #1849): `store_for_origin` may evict the origin's own oldest
    /// images, which a post-store rollback could not restore. Non-display
    /// paths store without placing and skip this.
    ///
    /// S4 (#1849): the budget is shared with virtual prototypes, so both
    /// deques count; eviction stays same-kind (a blit never evicts a
    /// prototype — a budget held entirely by prototypes refuses instead).
    pub fn check_placement_quota_for_origin(
        &self,
        origin: Option<u64>,
    ) -> Result<(), KittyPlacementError> {
        self.check_shared_placement_quota(origin, self.placement_count_for_origin(origin))
    }

    /// Virtual-prototype quota preflight for one origin (S4, #1849): the
    /// refusal half of [`KittyImageLayer::register_virtual_for_origin`]
    /// without mutation.
    ///
    /// The combined `a=T,U=1` form calls this before decoding/storing, so
    /// a quota refusal stores nothing and never discards FIFO-evicted
    /// images (same S5 atomicity rationale as
    /// [`KittyImageLayer::check_placement_quota_for_origin`]). Same-kind
    /// FIFO only: a prototype never evicts a visible blit — a budget held
    /// entirely by blits refuses instead.
    pub fn check_virtual_quota_for_origin(
        &self,
        origin: Option<u64>,
    ) -> Result<(), KittyPlacementError> {
        self.check_shared_placement_quota(origin, self.virtual_count_for_origin(origin))
    }

    /// Shared placement-budget admission check (S5 quotas, S4 sharing).
    ///
    /// `own_evictable` is the admitting kind's own-origin count
    /// (placements for blits, prototypes for virtual): pressure evicts
    /// the origin's own oldest entry of that kind first, and global
    /// pressure from other origins refuses with
    /// [`KittyPlacementError::QuotaExceeded`] instead of evicting a
    /// victim. When the origin's budget is held entirely by the other
    /// kind (`own_evictable == 0`), admission refuses rather than
    /// destroying another kind's entries. Decided before any mutation so
    /// a refusal evicts nothing.
    fn check_shared_placement_quota(
        &self,
        origin: Option<u64>,
        own_evictable: usize,
    ) -> Result<(), KittyPlacementError> {
        let own_total =
            self.placement_count_for_origin(origin) + self.virtual_count_for_origin(origin);
        let evict_own = own_total >= KITTY_PER_ORIGIN_MAX_PLACEMENTS && own_evictable > 0;
        let global_total = self.placements.len() + self.virtual_protos.len();
        let global_len_after = global_total.saturating_sub(usize::from(evict_own));
        if global_len_after >= KITTY_PLACE_MAX_ITEMS {
            return Err(KittyPlacementError::QuotaExceeded);
        }
        if own_total >= KITTY_PER_ORIGIN_MAX_PLACEMENTS && own_evictable == 0 {
            return Err(KittyPlacementError::QuotaExceeded);
        }
        Ok(())
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
    ///
    /// S4 (#1849): virtual prototypes follow the terminal-truth `ImageId`
    /// rule — an unpinned delete clears every prototype of the image on
    /// `origin` (any prototype still serves runs otherwise), while a
    /// pinned delete spares them and pin `0` clears nothing. The return
    /// sums removed blit placements and prototypes.
    pub fn delete_by_wire(
        &mut self,
        origin: Option<u64>,
        image_id: u32,
        placement: Option<u32>,
    ) -> usize {
        let before = self.placements.len() + self.virtual_protos.len();
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
                self.virtual_protos
                    .retain(|v| !(v.origin == origin && v.wire_image == image_id));
            }
        }
        // Issue #2875: relative children follow their parent, exactly like
        // terminal truth's cascade.
        self.cascade_relative_orphans();
        before - (self.placements.len() + self.virtual_protos.len())
    }

    /// Clears all images, placements, and virtual prototypes
    /// (alternate-screen entry).
    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.virtual_protos.clear();
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

    /// Scroll-adjusted, unclamped pixel rect for any placement, resolving
    /// relative (`P=`/`Q=`) anchors against their parents at paint time
    /// (issue #2875).
    ///
    /// Absolute placements are [`Self::placement_full_rect`] unchanged. A
    /// relative placement walks its parent chain (bounded by
    /// [`KITTY_PLACE_MAX_PARENT_DEPTH`]) to the absolute ancestor, takes
    /// that ancestor's *painted* rect — the same scroll-adjusted geometry
    /// every blit uses — and adds the accumulated `H=`/`V=` cell offsets in
    /// pixels; the child's own span (not the parent's) becomes the extent.
    /// Consequences, each a required behavior:
    ///
    /// - a scroll moves the child with the parent, because the parent rect
    ///   is recomputed per frame (the child's own anchor/base never enter);
    /// - a parent scrolled fully off the top hides the child;
    /// - a missing parent, a parent on another origin, a virtual prototype
    ///   (never in this deque), an unset/nonsensical link, a chain past the
    ///   depth bound (a wire-reachable cycle), or an offset that overflows
    ///   the pixel range all return `None`: no paint, never a panic.
    ///
    /// Pure: reads the bounded placement deque, allocates nothing.
    #[must_use]
    pub fn placement_full_rect_resolved(
        &self,
        placement: &KittyPlacement,
        metrics: CellMetrics,
        scrollback_now: usize,
    ) -> Option<RectPx> {
        if placement.parent.is_none() {
            return Self::placement_full_rect(placement, metrics, scrollback_now);
        }
        let mut ancestor = placement;
        let mut dx_cells: i64 = 0;
        let mut dy_cells: i64 = 0;
        let mut depth = 0u8;
        while let Some((parent_image, parent_placement)) = ancestor.parent {
            if depth >= KITTY_PLACE_MAX_PARENT_DEPTH {
                // Depth bound hit: either a cycle or an absurd chain.
                return None;
            }
            let parent =
                self.find_placement_for_origin(ancestor.origin, parent_image, parent_placement)?;
            dx_cells = dx_cells.checked_add(i64::from(ancestor.parent_offset.0))?;
            dy_cells = dy_cells.checked_add(i64::from(ancestor.parent_offset.1))?;
            ancestor = parent;
            depth = depth.saturating_add(1);
        }
        let base = Self::placement_full_rect(ancestor, metrics, scrollback_now)?;
        // Pixel origin: the ancestor's painted rect plus the accumulated
        // cell offsets. Checked, then range-checked: an unrepresentable
        // origin fails closed instead of saturating into a wrong rect.
        let x = i64::from(base.x).checked_add(dx_cells.checked_mul(i64::from(metrics.width))?)?;
        let y = i64::from(base.y).checked_add(dy_cells.checked_mul(i64::from(metrics.height))?)?;
        let x = i32::try_from(x).ok()?;
        let y = i32::try_from(y).ok()?;
        let w = u64::from(placement.cols) * u64::from(metrics.width);
        let h = u64::from(placement.rows) * u64::from(metrics.height);
        Some(RectPx::new(x, y, saturating_u32(w), saturating_u32(h)))
    }

    /// Origin-scoped wire lookup, oldest first — the presentation mirror of
    /// terminal truth's `PlacementStore::get` (insertion order, so a `Q=0`
    /// parent resolves to the same anonymous placement on both sides).
    fn find_placement_for_origin(
        &self,
        origin: Option<u64>,
        wire_image: u32,
        wire_placement: u32,
    ) -> Option<&KittyPlacement> {
        self.placements.iter().find(|p| {
            p.origin == origin && p.wire_image == wire_image && p.wire_placement == wire_placement
        })
    }

    /// Drops relative placements whose parent no longer exists, mirroring
    /// terminal truth's cascade (issue #2875).
    ///
    /// Without this a stale child could resurrect: re-placing a deleted
    /// parent under the same wire identity would make the old link resolve
    /// again, painting a child truth already dropped. Repeated until no
    /// orphan remains, so grandchildren die with their parent; bounded by
    /// the placement caps.
    fn cascade_relative_orphans(&mut self) {
        loop {
            let mut orphan = None;
            for entry in &self.placements {
                if let Some((parent_image, parent_placement)) = entry.parent {
                    if self
                        .find_placement_for_origin(entry.origin, parent_image, parent_placement)
                        .is_none()
                    {
                        orphan = Some(entry.id);
                        break;
                    }
                }
            }
            match orphan {
                Some(id) => self.placements.retain(|p| p.id != id),
                None => break,
            }
        }
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

/// Blends a packed source window over a destination frame in place
/// (S1 `a=c` `C=0`, #1849): straight-alpha src-over per pixel with
/// rounded integer math, no float, no allocation. `window` holds the
/// `w*h` source pixels row-major; `dest` is the full frame addressed at
/// `(x, y)` with row stride `width * 4`. The caller bounds the rect; all
/// indexing here stays inside both buffers by construction.
fn blend_rect_over(dest: &mut [u8], window: &[u8], width: u32, x: u32, y: u32, w: u32, h: u32) {
    let stride = (width as usize).saturating_mul(4);
    let row_bytes = (w as usize).saturating_mul(4);
    for row in 0..h {
        let dest_row = ((y as usize).saturating_add(row as usize)).saturating_mul(stride);
        let src_row = (row as usize).saturating_mul(row_bytes);
        for col in 0..w {
            let d = dest_row
                .saturating_add((x as usize).saturating_mul(4))
                .saturating_add((col as usize).saturating_mul(4));
            let s = src_row.saturating_add((col as usize).saturating_mul(4));
            let (sr, sg, sb, sa) = (
                u32::from(window[s]),
                u32::from(window[s + 1]),
                u32::from(window[s + 2]),
                u32::from(window[s + 3]),
            );
            let (dr, dg, db, da) = (
                u32::from(dest[d]),
                u32::from(dest[d + 1]),
                u32::from(dest[d + 2]),
                u32::from(dest[d + 3]),
            );
            // Source-over: out = src + dst * (1 - sa), in `u32` with
            // round-half-up. An opaque source covers exactly (replace
            // parity at the endpoints); a transparent source keeps dest.
            let w_src = sa;
            let w_dst = da.saturating_mul(255 - sa).saturating_add(127) / 255;
            let out_a = (w_src + w_dst).min(255);
            if out_a == 0 {
                dest[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let blend = |sc: u32, dc: u32| {
                ((sc.saturating_mul(w_src)
                    .saturating_add(dc.saturating_mul(w_dst)))
                .saturating_add(out_a / 2)
                    / out_a) as u8
            };
            dest[d] = blend(sr, dr);
            dest[d + 1] = blend(sg, dg);
            dest[d + 2] = blend(sb, db);
            dest[d + 3] = out_a as u8;
        }
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
// Texture-preparation mechanics (mirrored in the graphics extension, W-141)
// ---------------------------------------------------------------------------

// Nearest-neighbor scaling (`rasterize`, `rasterize_clipped`) and the
// per-frame blit budget (`KittyFrameBudget`) are mirrored in the
// graphics extension crate (which keeps its own cache copy).
// Core owns its bounded raster cache in [`crate::kitty_raster`]
// (`KittyRasterKey`/`KittyRasterCache` with the
// `KITTY_RASTER_CACHE_MAX_*` caps, S6 #1849). The ceilings stay Core-owned
// above (`KITTY_PRESENT_MAX_*`) so the present layer keeps enforcing
// identical bounds.

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
    fn placement_cap_evicts_oldest_within_origin() {
        // S5 (#1849): the placement quota is per origin (32). One origin
        // placing past it evicts its own oldest first; other origins are
        // untouched (cross-origin cases live in the per-origin quota
        // integration suite).
        let mut layer = KittyImageLayer::new();
        let id = stored_red(&mut layer);
        let mut first = None;
        for i in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS + 5 {
            let pid = layer.display(id, 0, 0, 1, 1, METRICS, 0, i as i32).unwrap();
            if i == 0 {
                first = Some(pid);
            }
        }
        assert_eq!(layer.placement_len(), KITTY_PER_ORIGIN_MAX_PLACEMENTS);
        assert!(layer.get_placement(first.unwrap()).is_none());
    }

    #[test]
    fn caps_match_retained_policy_values() {
        // Compile-time: placement enforces exactly the decode ceilings.
        const _: () = assert!(KITTY_PLACE_MAX_BYTES == 256 * 1024 * 1024);
        const _: () = assert!(KITTY_PLACE_MAX_ITEMS == 128);
        // S5 (#1849): per-origin quotas live inside the global caps.
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_IMAGES == 64);
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_BYTES == 64 * 1024 * 1024);
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_PLACEMENTS == 32);
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_IMAGES == KITTY_PLACE_MAX_IMAGES);
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_BYTES * 4 == KITTY_PLACE_MAX_BYTES);
        const _: () = assert!(KITTY_PER_ORIGIN_MAX_PLACEMENTS * 4 == KITTY_PLACE_MAX_ITEMS);
        assert_eq!(
            KITTY_PLACE_MAX_IMAGES,
            bitty_term_state::IMAGE_STORE_MAX_ENTRIES
        );
        // Error strings stay stable for snapshot greps.
        assert_eq!(
            KittyPlacementError::ImageNotFound(KittyImageId(7)).to_string(),
            "kitty placement image not found: 7"
        );
        assert_eq!(
            KittyPlacementError::QuotaExceeded.to_string(),
            "kitty quota exceeded: global bound held by other origins, refused without evicting"
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
        const _: () = assert!(KITTY_DECODE_MAX_BYTES * 4 < KITTY_LEDGER_MAX_BYTES);
        assert_eq!(KITTY_LEDGER_MAX_BYTES, 320_000_000);
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
    fn virtual_prototype_registers_and_resolves() {
        // S4 (#1849): a bodiless `a=p,U=1` registers an origin-tagged
        // prototype keyed by wire `i=`/`p=`; no blit placement exists.
        let mut layer = KittyImageLayer::new();
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0)
            .expect("first prototype must register");
        assert_eq!(layer.virtual_len(), 1);
        assert_eq!(layer.virtual_count_for_origin(None), 1);
        assert!(layer.placement_is_empty(), "prototypes never emit blits");
        assert!(layer.placements_in_paint_order_for(None).is_empty());
        let proto = layer.get_virtual(None, 42, None).expect("held");
        assert_eq!(
            *proto,
            KittyVirtualPrototype {
                origin: None,
                wire_image: 42,
                wire_placement: 0,
                image: None,
                cols: 4,
                rows: 2,
                image_width: 0,
                image_height: 0,
                scrollback_base: 0,
            }
        );
        // Wire `p=0` is anonymous: the sizing view carries no placement.
        assert_eq!(proto.as_unicode_virtual().placement_id, None);
        // Other origins and images resolve to nothing.
        assert!(layer.get_virtual(Some(7), 42, None).is_none());
        assert!(layer.get_virtual(None, 43, None).is_none());
        assert!(layer.get_virtual(None, 42, Some(0)).is_none());
    }

    #[test]
    fn virtual_prototype_reput_replaces_addressed() {
        // S4 (#1849): re-putting the same addressed `(origin, i, p)` key
        // replaces in place (terminal-truth `upsert` parity); anonymous
        // prototypes accumulate.
        let mut layer = KittyImageLayer::new();
        layer
            .register_virtual_for_origin(None, 42, 9, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer
            .register_virtual_for_origin(None, 42, 9, None, 6, 3, 0, 0, 5)
            .unwrap();
        assert_eq!(layer.virtual_len(), 1, "addressed re-put replaces");
        let proto = layer.get_virtual(None, 42, Some(9)).expect("replaced");
        assert_eq!((proto.cols, proto.rows), (6, 3));
        assert_eq!(proto.scrollback_base, 5);
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        assert_eq!(layer.virtual_len(), 3, "anonymous protos accumulate");
    }

    #[test]
    fn virtual_prototype_per_origin_quota_evicts_own_oldest() {
        // S4 (#1849): prototypes share the 32-per-origin placement quota;
        // pressure evicts the origin's own oldest prototype first.
        let mut layer = KittyImageLayer::new();
        for i in 1..=KITTY_PER_ORIGIN_MAX_PLACEMENTS + 5 {
            layer
                .register_virtual_for_origin(None, 100 + i as u32, 0, None, 4, 2, 0, 0, 0)
                .unwrap();
        }
        assert_eq!(layer.virtual_len(), KITTY_PER_ORIGIN_MAX_PLACEMENTS);
        assert!(
            layer.get_virtual(None, 101, None).is_none(),
            "oldest own prototype evicted"
        );
        assert!(layer.get_virtual(None, 106, None).is_some());
        // Another origin is untouched by the pressure.
        layer
            .register_virtual_for_origin(Some(7), 7, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        assert!(layer.get_virtual(Some(7), 7, None).is_some());
    }

    #[test]
    fn virtual_prototype_global_pressure_refuses_without_evicting() {
        // S4 (#1849): when the shared 128 global placement bound is held
        // by other origins, admission refuses with `QuotaExceeded`
        // instead of evicting a victim (S5 no-cross-origin-eviction).
        let mut layer = KittyImageLayer::new();
        for origin in 0..4u64 {
            for i in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS {
                layer
                    .register_virtual_for_origin(
                        Some(origin),
                        1000 + (origin as u32) * 100 + i as u32,
                        0,
                        None,
                        4,
                        2,
                        0,
                        0,
                        0,
                    )
                    .unwrap();
            }
        }
        assert_eq!(layer.virtual_len(), KITTY_PLACE_MAX_ITEMS);
        assert_eq!(
            layer.register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0),
            Err(KittyPlacementError::QuotaExceeded),
            "over-cap prototype refused"
        );
        assert_eq!(layer.virtual_len(), KITTY_PLACE_MAX_ITEMS);
    }

    #[test]
    fn virtual_prototypes_share_the_placement_budget() {
        // S4 (#1849): prototypes consume the same 32-per-origin budget as
        // blit placements; same-kind FIFO only, never cross-kind eviction.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        for _ in 0..KITTY_PER_ORIGIN_MAX_PLACEMENTS {
            layer.display(img, 0, 0, 1, 1, METRICS, 0, 0).unwrap();
        }
        assert_eq!(
            layer.register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0),
            Err(KittyPlacementError::QuotaExceeded),
            "blit-held budget refuses prototypes without evicting blits"
        );
        assert_eq!(layer.placement_len(), KITTY_PER_ORIGIN_MAX_PLACEMENTS);
        let mut layer = KittyImageLayer::new();
        for i in 1..=KITTY_PER_ORIGIN_MAX_PLACEMENTS as u32 {
            layer
                .register_virtual_for_origin(None, 500 + i, 0, None, 4, 2, 0, 0, 0)
                .unwrap();
        }
        let img = stored_red(&mut layer);
        assert_eq!(
            layer.display(img, 0, 0, 1, 1, METRICS, 0, 0),
            Err(KittyPlacementError::QuotaExceeded),
            "proto-held budget refuses blits without evicting protos"
        );
        assert_eq!(layer.virtual_len(), KITTY_PER_ORIGIN_MAX_PLACEMENTS);
    }

    #[test]
    fn virtual_prototype_delete_by_wire_mirrors_truth_selectors() {
        // S4 (#1849): terminal truth kills prototypes only to unpinned id
        // selectors (`placement.rs` ImageId arm); pinned deletes spare
        // them, and pin `0` names nothing.
        let mut layer = KittyImageLayer::new();
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer
            .register_virtual_for_origin(None, 42, 9, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer
            .register_virtual_for_origin(Some(7), 42, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        assert_eq!(layer.delete_by_wire(None, 42, Some(9)), 0);
        assert_eq!(layer.virtual_len(), 3, "pinned delete spares prototypes");
        assert_eq!(layer.delete_by_wire(None, 42, Some(0)), 0);
        assert_eq!(layer.delete_by_wire(None, 42, None), 2);
        assert_eq!(layer.virtual_len(), 1, "unpinned clears origin protos");
        assert!(layer.get_virtual(Some(7), 42, None).is_some());
    }

    #[test]
    fn virtual_prototype_clear_scopes_match_truth() {
        // S4 (#1849): `d=a`/alt-suppression (`clear_origin`) keeps
        // prototypes like terminal-truth `AllVisible`; ED2/FullReset
        // (`clear_virtual_for_origin`) drops them like `clear_screen`;
        // pane teardown (`retire_origin`) and total `clear` drop them.
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        layer.display(img, 0, 0, 1, 1, METRICS, 0, 0).unwrap();
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer.clear_origin(None);
        assert!(layer.placement_is_empty(), "blits cleared");
        assert_eq!(layer.virtual_len(), 1, "prototypes survive d=a/alt");
        layer.clear_virtual_for_origin(None);
        assert_eq!(layer.virtual_len(), 0, "ED2 clears prototypes");
        assert_eq!(layer.len(), 1, "images stay inert");
        layer
            .register_virtual_for_origin(Some(7), 7, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer.retire_origin(Some(7));
        assert!(layer.get_virtual(Some(7), 7, None).is_none());
        layer
            .register_virtual_for_origin(None, 9, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        layer.clear();
        assert_eq!(layer.virtual_len(), 0);
        assert!(layer.is_empty());
    }

    #[test]
    fn virtual_prototype_dies_with_evicted_image() {
        // S4 (#1849): a combined-form prototype links its stored image;
        // store eviction and explicit removal cascade like blit
        // placements. Bodiless prototypes (`None`) have no link.
        let mut layer = KittyImageLayer::new();
        let first = layer.store(2, 2, vec![1; 16], 16).unwrap();
        layer
            .register_virtual_for_origin(None, 42, 0, Some(first), 4, 2, 2, 2, 0)
            .unwrap();
        layer
            .register_virtual_for_origin(None, 43, 0, None, 4, 2, 0, 0, 0)
            .unwrap();
        assert!(layer.remove(first));
        assert!(
            layer.get_virtual(None, 42, None).is_none(),
            "linked prototype cascades"
        );
        assert!(
            layer.get_virtual(None, 43, None).is_some(),
            "bodiless prototype survives"
        );
        // Store-pressure eviction cascades the same way: filling the
        // origin's image quota evicts the oldest image and its linked
        // prototype, while the bodiless one survives.
        let mut layer = KittyImageLayer::new();
        let first = layer.store(2, 2, vec![1; 16], 16).unwrap();
        layer
            .register_virtual_for_origin(None, 42, 0, Some(first), 4, 2, 2, 2, 0)
            .unwrap();
        for _ in 0..KITTY_PER_ORIGIN_MAX_IMAGES {
            layer.store(2, 2, vec![2; 16], 16).unwrap();
        }
        assert!(layer.get(first).is_none(), "oldest image evicted");
        assert!(
            layer.get_virtual(None, 42, None).is_none(),
            "linked prototype cascades on store eviction"
        );
    }

    #[test]
    fn virtual_run_rect_links_runs_to_prototype() {
        // S4 (#1849): the two-layer link resolves a grid run against the
        // registered prototype; scroll follows the `scrollback_base`
        // delta; mismatches and foreign origins resolve to nothing.
        use bitty_term_state::{KittyUnicodeCell, KittyUnicodeId, KittyUnicodeRun};
        let mut layer = KittyImageLayer::new();
        layer
            .register_virtual_for_origin(None, 42, 0, None, 4, 2, 32, 32, 100)
            .unwrap();
        let cells = vec![
            KittyUnicodeCell {
                grid_row: 10,
                grid_col: 3,
                id: KittyUnicodeId {
                    image_id: 42,
                    placement_id: None,
                    row: 0,
                    col: 0,
                },
            },
            KittyUnicodeCell {
                grid_row: 10,
                grid_col: 4,
                id: KittyUnicodeId {
                    image_id: 42,
                    placement_id: None,
                    row: 0,
                    col: 1,
                },
            },
        ];
        let run = KittyUnicodeRun {
            image_id: 42,
            placement_id: None,
            row: 0,
            col: 0,
            grid_row: 10,
            grid_col: 3,
            width: 2,
        };
        let rect = layer
            .virtual_run_rect(None, &run, &cells, METRICS, 100)
            .expect("run resolves against the prototype");
        assert_eq!((rect.grid_row, rect.grid_col), (10, 3));
        assert_eq!((rect.cols, rect.rows), (2, 1));
        assert_eq!(rect.rect, RectPx::new(3 * 8, 10 * 16, 2 * 8, 16));
        // Scroll delta: three lines scrolled moves the rect up three rows.
        let scrolled = layer
            .virtual_run_rect(None, &run, &cells, METRICS, 103)
            .expect("scrolled rect");
        assert_eq!(scrolled.rect.y, (10 - 3) * 16);
        // Scrolled fully off the top paints nothing.
        assert!(
            layer
                .virtual_run_rect(None, &run, &cells, METRICS, 112)
                .is_none()
        );
        // A run naming another image resolves to nothing.
        let other = KittyUnicodeRun {
            image_id: 43,
            ..run
        };
        assert!(
            layer
                .virtual_run_rect(None, &other, &cells, METRICS, 100)
                .is_none()
        );
        // Prototypes are origin-scoped: a foreign origin resolves nothing.
        assert!(
            layer
                .virtual_run_rect(Some(7), &run, &cells, METRICS, 100)
                .is_none()
        );
        assert!(
            layer
                .virtual_run_rect(None, &run, &[], METRICS, 100)
                .is_none()
        );
    }

    #[test]
    fn fit_aspect_rect_letterboxes_and_centers() {
        // Square into a 90x95 rect: full width, 90 tall, 2px top inset.
        assert_eq!(
            fit_aspect_rect(RectPx::new(10, 20, 90, 95), 8, 8),
            RectPx::new(10, 22, 90, 90)
        );
        // 4:1 image into 100x50: full width, 25 tall, 12px top inset.
        assert_eq!(
            fit_aspect_rect(RectPx::new(0, 0, 100, 50), 200, 50),
            RectPx::new(0, 12, 100, 25)
        );
        // Tall image into 100x50: full height, 12 wide, 44px left inset.
        assert_eq!(
            fit_aspect_rect(RectPx::new(0, 0, 100, 50), 50, 200),
            RectPx::new(44, 0, 12, 50)
        );
        // Exact aspect is a no-op; degenerate inputs fail open to `outer`.
        assert_eq!(
            fit_aspect_rect(RectPx::new(5, 6, 40, 20), 4, 2),
            RectPx::new(5, 6, 40, 20)
        );
        assert_eq!(
            fit_aspect_rect(RectPx::new(5, 6, 40, 20), 0, 2),
            RectPx::new(5, 6, 40, 20)
        );
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

    /// Relative placements (issue #2875): the resolver derives the child rect
    /// from the parent's painted origin plus the wire `H=`/`V=` cell offsets,
    /// and re-derives it per frame so a scroll carries the child along.
    #[test]
    fn relative_placement_resolves_parent_origin_plus_offsets_and_scroll() {
        let mut layer = KittyImageLayer::new();
        let parent_img = stored_red(&mut layer);
        let child_img = stored_red(&mut layer);
        let parent = layer
            .display_for_origin_with_wire(parent_img, 4, 10, 2, 2, METRICS, 0, 0, None, 7, 1)
            .expect("parent places");
        let child = layer
            .display_relative_for_origin(child_img, 2, 2, METRICS, 0, 0, None, 8, 2, (7, 1), (3, 1))
            .expect("relative child places");
        let parent_placement = layer.get_placement(parent).unwrap();
        let child_placement = layer.get_placement(child).unwrap();
        let base = KittyImageLayer::placement_full_rect(parent_placement, METRICS, 0).unwrap();
        assert_eq!(base, RectPx::new(4 * 8, 10 * 16, 2 * 8, 2 * 16));
        let resolved = layer
            .placement_full_rect_resolved(child_placement, METRICS, 0)
            .expect("child resolves");
        assert_eq!(
            resolved,
            RectPx::new(base.x + 3 * 8, base.y + 16, 2 * 8, 2 * 16),
            "child paints at parent origin + H=3 / V=1 cells with its own span"
        );
        // Four scrolled rows move the parent up four cells; the child keeps
        // its offset with no scroll state of its own.
        let scrolled_parent = layer
            .placement_full_rect_resolved(parent_placement, METRICS, 4)
            .unwrap();
        let scrolled_child = layer
            .placement_full_rect_resolved(child_placement, METRICS, 4)
            .unwrap();
        assert_eq!(scrolled_parent.y, base.y - 4 * 16);
        assert_eq!(
            scrolled_child,
            RectPx::new(
                scrolled_parent.x + 3 * 8,
                scrolled_parent.y + 16,
                resolved.width,
                resolved.height
            )
        );
    }

    /// Fail-closed paths (issue #2875): a missing parent and an offset the
    /// pixel range cannot hold both paint nothing.
    #[test]
    fn relative_placement_fails_closed_on_missing_parent_and_overflow() {
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        // Missing parent: nothing holds wire `(99, 9)`.
        let orphan = layer
            .display_relative_for_origin(img, 2, 2, METRICS, 0, 0, None, 8, 2, (99, 9), (0, 0))
            .expect("admission does not resolve parents");
        let orphan_placement = layer.get_placement(orphan).unwrap();
        assert!(
            layer
                .placement_full_rect_resolved(orphan_placement, METRICS, 0)
                .is_none()
        );

        // Offset overflow: `H=i32::MAX` cells cannot land in an `i32` origin.
        let parent = layer
            .display_for_origin_with_wire(img, 0, 0, 2, 2, METRICS, 0, 0, None, 7, 1)
            .expect("parent places");
        let far = layer
            .display_relative_for_origin(
                img,
                2,
                2,
                METRICS,
                0,
                0,
                None,
                8,
                3,
                (7, 1),
                (i32::MAX, 0),
            )
            .expect("admission does not resolve offsets");
        assert_eq!(
            layer.get_placement(far).unwrap().parent,
            Some((7, 1)),
            "admission keeps the wire link verbatim"
        );
        assert!(
            layer
                .placement_full_rect_resolved(layer.get_placement(far).unwrap(), METRICS, 0)
                .is_none()
        );
        assert!(layer.get_placement(parent).is_some());
    }

    /// Fail-closed path (issue #2875): a parent cycle is reachable when a
    /// relative placement is admitted before the placement it names, so the
    /// resolver must stop at the depth bound instead of looping.
    #[test]
    fn relative_placement_cycle_hits_the_depth_bound() {
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        // `(10, 1)` names `(11, 2)` before it exists; `(11, 2)` then names
        // `(10, 1)`: both links resolve and the chain closes on itself.
        let first = layer
            .display_relative_for_origin(img, 2, 2, METRICS, 0, 0, None, 10, 1, (11, 2), (1, 0))
            .expect("first link admits");
        let second = layer
            .display_relative_for_origin(img, 2, 2, METRICS, 0, 0, None, 11, 2, (10, 1), (1, 0))
            .expect("second link admits");
        for id in [first, second] {
            assert!(
                layer
                    .placement_full_rect_resolved(layer.get_placement(id).unwrap(), METRICS, 0)
                    .is_none(),
                "a cyclic chain must paint nothing"
            );
        }
    }

    /// Depth bound (issue #2875): a chain of exactly
    /// [`KITTY_PLACE_MAX_PARENT_DEPTH`] links resolves; one more link refuses.
    #[test]
    fn relative_placement_chain_stops_at_the_depth_bound() {
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        let max = u32::from(KITTY_PLACE_MAX_PARENT_DEPTH);
        layer
            .display_for_origin_with_wire(img, 0, 0, 1, 1, METRICS, 0, 0, None, 1, 1)
            .expect("root places");
        for level in 2..=max + 2 {
            layer
                .display_relative_for_origin(
                    img,
                    1,
                    1,
                    METRICS,
                    0,
                    0,
                    None,
                    1,
                    level,
                    (1, level - 1),
                    (1, 0),
                )
                .expect("link admits");
        }
        // Placement ids start at 1, so the level-`n` placement is id `n`:
        // id `max + 1` sits exactly `max` links below the root and still
        // resolves ...
        let deepest = layer
            .get_placement(KittyPlacementId(u64::from(max) + 1))
            .expect("level `max + 1` admits");
        assert!(
            layer
                .placement_full_rect_resolved(deepest, METRICS, 0)
                .is_some(),
            "a chain of exactly the depth bound resolves"
        );
        // ... while one more link is past it.
        let beyond = layer
            .get_placement(KittyPlacementId(u64::from(max) + 2))
            .expect("level `max + 2` admits");
        assert!(
            layer
                .placement_full_rect_resolved(beyond, METRICS, 0)
                .is_none(),
            "a chain one link past the bound fails closed"
        );
    }

    /// Re-put vs delete (issue #2875): an addressed re-put replaces the
    /// parent in place, so its relative children keep resolving against the
    /// new origin; a protocol delete cascades them away with it.
    #[test]
    fn relative_child_survives_a_parent_reput_but_not_a_delete() {
        let mut layer = KittyImageLayer::new();
        let img = stored_red(&mut layer);
        layer
            .display_for_origin_with_wire(img, 0, 0, 2, 2, METRICS, 0, 0, None, 7, 1)
            .expect("parent places");
        let child = layer
            .display_relative_for_origin(img, 1, 1, METRICS, 0, 0, None, 8, 2, (7, 1), (1, 1))
            .expect("child places");
        let child_of = |layer: &KittyImageLayer| {
            layer
                .placement_full_rect_resolved(layer.get_placement(child).unwrap(), METRICS, 0)
                .map(|rect| (rect.x, rect.y))
        };
        assert_eq!(child_of(&layer), Some((8, 16)));
        // Same wire identity, new anchor: replace in place, not delete+add.
        let replaced = layer
            .display_for_origin_with_wire(img, 3, 3, 2, 2, METRICS, 0, 0, None, 7, 1)
            .expect("parent re-put");
        assert_eq!(layer.placement_len(), 2, "a re-put must not duplicate");
        assert_eq!(
            child_of(&layer),
            Some((3 * 8 + 8, 3 * 16 + 16)),
            "the child follows its re-put parent instead of being dropped"
        );
        assert_eq!(
            layer.get_placement(replaced).map(|p| p.anchor_col),
            Some(3),
            "the re-put keeps the placement id and takes the new anchor"
        );
        // A delete takes the parent and its child (terminal-truth cascade).
        assert_eq!(layer.delete_by_wire(None, 7, Some(1)), 2);
        assert_eq!(layer.placement_len(), 0);
    }
}
