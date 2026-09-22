//! Complex-widget mechanism split (UX-17, CTX-0668).
//!
//! Candidate implementation of U-3
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending UI Runtime RFC). Nothing here is normative,
//! accepted, or verified: every bound, struct, and rule below is a candidate
//! spelling that the UI Runtime RFC accepts or rejects, never this module.
//! The module is English-only.
//!
//! The split: for complex widgets, Rust owns the **mechanism** and Lua owns
//! the **appearance**.
//!
//! - Rust (this module): virtualization windows, IME composition state,
//!   scroll offsets, canvas command budgets. Pure integer/string state,
//!   deterministic, headless, bounded; fails closed on every bound.
//! - Lua (elsewhere): per-item rendering, colors, fonts, spacing —
//!   everything a pixel or a typeface touches. This module carries no
//!   color, font, glyph, pixel buffer, or raster handle.
//!
//! Each mechanism binds the canonical
//! [`UiNodeId`](crate::uitree::UiNodeId) of its tree node (never redefined
//! here): mechanism state reconciles by the same stable identity the
//! retained tree diffs by. Text payloads reuse the canonical
//! [`MAX_UI_TEXT_LEN`](crate::uitree::MAX_UI_TEXT_LEN) cap so one bound
//! governs every text surface.
//!
//! No render, platform, PTY, exec, or plugin coupling: nothing here draws,
//! spawns, or dispatches. The `Terminal` primitive stays a presentation
//! attachment with no handle (see [`crate::uitree`]).
//!
//! Plugin-migration follow-up: beacon (U-8) is plugin-future. If complex
//! widgets later move behind a plugin boundary, these headless mechanism
//! structs migrate as the boundary state with no render/exec residue to
//! untangle; appearance stays Lua-side either way.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, or platform handle participates.

#![forbid(unsafe_code)]

use std::fmt;
use std::ops::Range;

use crate::uitree::{MAX_UI_TEXT_LEN, UiNodeId};

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on virtualized items in one [`VirtualListMech`].
///
/// Rejected with [`WidgetMechError::TooManyItems`], never silently pruned.
pub const MAX_VIRTUAL_ITEMS: usize = 100_000;

/// Hard cap in pixels on one virtualized row height.
pub const MAX_ITEM_HEIGHT_PX: u32 = 4096;

/// Hard cap in pixels on a mechanism viewport extent.
pub const MAX_VIEWPORT_PX: u32 = 16_384;

/// Hard cap in pixels on scrollable content length.
pub const MAX_SCROLL_CONTENT_PX: u64 = 1_048_576;

/// Hard cap in pixels on one canvas dimension.
pub const MAX_MECH_CANVAS_DIM_PX: u32 = 16_384;

/// Hard cap on queued canvas display-list commands per node.
///
/// Rejected with [`WidgetMechError::CommandBudgetExceeded`]: an unbounded
/// display list is an unbounded paint budget.
pub const MAX_MECH_CANVAS_COMMANDS: u32 = 65_536;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build or drive widget mechanism state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WidgetMechError {
    /// A virtualized list holds more than [`MAX_VIRTUAL_ITEMS`] items.
    TooManyItems {
        /// Items requested.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A row height is zero or exceeds [`MAX_ITEM_HEIGHT_PX`].
    BadItemHeight {
        /// Height requested in pixels.
        found: u32,
    },
    /// A viewport extent is zero or exceeds [`MAX_VIEWPORT_PX`].
    BadViewport {
        /// Extent requested in pixels.
        found: u64,
    },
    /// Scrollable content exceeds [`MAX_SCROLL_CONTENT_PX`].
    BadScrollContent {
        /// Length requested in pixels.
        found: u64,
        /// The cap that was exceeded.
        cap: u64,
    },
    /// A canvas dimension is zero or exceeds [`MAX_MECH_CANVAS_DIM_PX`].
    BadCanvasSize {
        /// Width requested in pixels.
        width: u32,
        /// Height requested in pixels.
        height: u32,
    },
    /// Queued canvas commands would exceed [`MAX_MECH_CANVAS_COMMANDS`].
    CommandBudgetExceeded {
        /// Commands that would be queued.
        requested: u64,
        /// The cap that was exceeded.
        cap: u64,
    },
    /// A text payload (value, placeholder, or IME preedit) exceeds
    /// [`MAX_UI_TEXT_LEN`] characters.
    TextTooLong {
        /// Length in characters of the rejected payload.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
}

impl fmt::Display for WidgetMechError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyItems { found, cap } => {
                write!(f, "virtual list too large: {found} items exceeds cap {cap}")
            }
            Self::BadItemHeight { found } => {
                write!(f, "bad virtual row height: {found}px")
            }
            Self::BadViewport { found } => {
                write!(f, "bad mechanism viewport: {found}px")
            }
            Self::BadScrollContent { found, cap } => {
                write!(f, "scroll content too long: {found}px exceeds cap {cap}px")
            }
            Self::BadCanvasSize { width, height } => {
                write!(f, "bad canvas size: {width}x{height}px")
            }
            Self::CommandBudgetExceeded { requested, cap } => {
                write!(
                    f,
                    "canvas command budget exceeded: {requested} exceeds cap {cap}"
                )
            }
            Self::TextTooLong { len, cap } => {
                write!(f, "widget text too long: {len} chars exceeds cap {cap}")
            }
        }
    }
}

impl std::error::Error for WidgetMechError {}

fn check_text_len(text: &str) -> Result<(), WidgetMechError> {
    let len = text.chars().count();
    if len > MAX_UI_TEXT_LEN {
        return Err(WidgetMechError::TextTooLong {
            len,
            cap: MAX_UI_TEXT_LEN,
        });
    }
    Ok(())
}

fn check_viewport(px: u64) -> Result<(), WidgetMechError> {
    if px == 0 || px > u64::from(MAX_VIEWPORT_PX) {
        return Err(WidgetMechError::BadViewport { found: px });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// VirtualList mechanism: virtualization window (Rust), appearance (Lua)
// ---------------------------------------------------------------------------

/// Rust-owned virtualization state for one `VirtualList` node.
///
/// Lua defines per-item appearance and action dispatch; Rust instantiates
/// only the visible window. This struct carries indices and offsets only —
/// no item content, color, or font.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VirtualListMech {
    node: UiNodeId,
    item_count: usize,
    item_height_px: u32,
    viewport_height_px: u32,
    scroll_offset_px: u64,
}

impl VirtualListMech {
    /// Builds virtualization state, clamped to a valid scrolled-to-top view.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError`] when the item count, row height, or
    /// viewport violates its bound.
    pub fn new(
        node: UiNodeId,
        item_count: usize,
        item_height_px: u32,
        viewport_height_px: u32,
    ) -> Result<Self, WidgetMechError> {
        if item_count > MAX_VIRTUAL_ITEMS {
            return Err(WidgetMechError::TooManyItems {
                found: item_count,
                cap: MAX_VIRTUAL_ITEMS,
            });
        }
        if item_height_px == 0 || item_height_px > MAX_ITEM_HEIGHT_PX {
            return Err(WidgetMechError::BadItemHeight {
                found: item_height_px,
            });
        }
        check_viewport(u64::from(viewport_height_px))?;
        Ok(Self {
            node,
            item_count,
            item_height_px,
            viewport_height_px,
            scroll_offset_px: 0,
        })
    }

    /// The tree node this mechanism drives.
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    /// Total virtualized items.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.item_count
    }

    /// Current scroll offset in pixels (always `<= max_offset`).
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.scroll_offset_px
    }

    /// Total content height in pixels.
    #[must_use]
    pub fn content_height_px(&self) -> u64 {
        (self.item_count as u64).saturating_mul(u64::from(self.item_height_px))
    }

    /// Largest valid scroll offset: content minus viewport, floored at
    /// zero when the content fits.
    #[must_use]
    pub fn max_offset(&self) -> u64 {
        self.content_height_px()
            .saturating_sub(u64::from(self.viewport_height_px))
    }

    /// Sets the scroll offset, clamped to `0..=max_offset`.
    pub fn set_offset(&mut self, offset_px: u64) {
        self.scroll_offset_px = offset_px.min(self.max_offset());
    }

    /// Moves the scroll offset by a signed delta, saturating at both ends
    /// and clamped to `max_offset`.
    pub fn scroll_by(&mut self, delta_px: i64) {
        let next = (self.scroll_offset_px as i64)
            .saturating_add(delta_px)
            .max(0) as u64;
        self.set_offset(next);
    }

    /// Pixel offset of the top of `index`, or `None` when out of range.
    #[must_use]
    pub fn item_offset(&self, index: usize) -> Option<u64> {
        if index >= self.item_count {
            return None;
        }
        Some((index as u64).saturating_mul(u64::from(self.item_height_px)))
    }

    /// Half-open index window to instantiate: every row intersecting
    /// `[offset, offset + viewport)`, clamped to `item_count`.
    ///
    /// Pure integer division with ceiling on the trailing edge, so a
    /// partially visible row is included and an empty list yields `0..0`.
    #[must_use]
    pub fn visible_range(&self) -> Range<usize> {
        let item_h = u64::from(self.item_height_px);
        let start = (self.scroll_offset_px / item_h).min(self.item_count as u64) as usize;
        let end = self
            .scroll_offset_px
            .saturating_add(u64::from(self.viewport_height_px))
            .saturating_add(item_h.saturating_sub(1))
            / item_h;
        let end = end.min(self.item_count as u64) as usize;
        start.min(end)..end
    }

    /// Scrolls the minimum distance that makes `index` fully visible.
    /// Out-of-range indices clamp to the nearest valid row; an empty list
    /// is a no-op.
    pub fn ensure_visible(&mut self, index: usize) {
        if self.item_count == 0 {
            return;
        }
        let index = index.min(self.item_count.saturating_sub(1));
        let Some(top) = self.item_offset(index) else {
            return;
        };
        let bottom = top.saturating_add(u64::from(self.item_height_px));
        if top < self.scroll_offset_px {
            self.set_offset(top);
        } else if bottom
            > self
                .scroll_offset_px
                .saturating_add(u64::from(self.viewport_height_px))
        {
            self.set_offset(bottom.saturating_sub(u64::from(self.viewport_height_px)));
        }
    }
}

// ---------------------------------------------------------------------------
// TextInput mechanism: IME composition state (Rust), styling (Lua)
// ---------------------------------------------------------------------------

/// Rust-owned text composition state for one `TextInput` node.
///
/// Owns the value, the character-index cursor, and the in-progress IME
/// preedit. Styling, placeholder tint, and key routing policy stay
/// Lua-side; this struct never sees a key event, only its effects.
/// All edits are character-based: byte splits of multi-byte sequences are
/// unreachable by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInputMech {
    node: UiNodeId,
    value: String,
    cursor: usize,
    preedit: String,
}

impl TextInputMech {
    /// Builds composition state with the cursor at the end of `value`.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::TextTooLong`] when `value` exceeds
    /// [`MAX_UI_TEXT_LEN`] characters.
    pub fn new(node: UiNodeId, value: String) -> Result<Self, WidgetMechError> {
        check_text_len(&value)?;
        let cursor = value.chars().count();
        Ok(Self {
            node,
            value,
            cursor,
            preedit: String::new(),
        })
    }

    /// The tree node this mechanism drives.
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    /// Current committed value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Cursor as a character index into [`Self::value`].
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// In-progress IME preedit (uncommitted).
    #[must_use]
    pub fn preedit(&self) -> &str {
        &self.preedit
    }

    /// Byte index of the cursor; always a character boundary.
    fn cursor_byte(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.value.len())
    }

    /// Inserts `text` at the cursor and advances past it.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::TextTooLong`] without mutating when the
    /// combined value would exceed [`MAX_UI_TEXT_LEN`] characters.
    pub fn insert(&mut self, text: &str) -> Result<(), WidgetMechError> {
        let combined = self
            .value
            .chars()
            .count()
            .saturating_add(text.chars().count());
        if combined > MAX_UI_TEXT_LEN {
            return Err(WidgetMechError::TextTooLong {
                len: combined,
                cap: MAX_UI_TEXT_LEN,
            });
        }
        let byte = self.cursor_byte();
        self.value.insert_str(byte, text);
        self.cursor = self.cursor.saturating_add(text.chars().count());
        Ok(())
    }

    /// Moves the cursor by a signed character delta, clamped to the value.
    pub fn move_cursor(&mut self, delta: i32) {
        let len = self.value.chars().count() as i64;
        let next = (self.cursor as i64)
            .saturating_add(i64::from(delta))
            .clamp(0, len);
        self.cursor = next as usize;
    }

    /// Deletes the character before the cursor. Returns `false` (no-op)
    /// at the start of the value.
    pub fn delete_before(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let byte = self
            .value
            .char_indices()
            .nth(self.cursor.saturating_sub(1))
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.value.remove(byte);
        self.cursor = self.cursor.saturating_sub(1);
        true
    }

    /// Replaces the in-progress IME preedit.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::TextTooLong`] without mutating when the
    /// preedit exceeds [`MAX_UI_TEXT_LEN`] characters.
    pub fn set_preedit(&mut self, preedit: String) -> Result<(), WidgetMechError> {
        check_text_len(&preedit)?;
        self.preedit = preedit;
        Ok(())
    }

    /// Commits the preedit at the cursor and clears it.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::TextTooLong`] without mutating when the
    /// committed value would exceed [`MAX_UI_TEXT_LEN`] characters.
    pub fn commit_preedit(&mut self) -> Result<(), WidgetMechError> {
        if self.preedit.is_empty() {
            return Ok(());
        }
        let committed = std::mem::take(&mut self.preedit);
        if let Err(err) = self.insert(&committed) {
            self.preedit = committed;
            return Err(err);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ScrollView mechanism: scroll physics state (Rust), visuals (Lua)
// ---------------------------------------------------------------------------

/// Rust-owned scroll offset state for one `ScrollView` node.
///
/// Owns clamping and signed deltas over a fixed content length.
/// Overscroll effects, scrollbar visuals, and fling curves stay Lua-side;
/// this struct tracks the settled offset only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ScrollMech {
    node: UiNodeId,
    content_len_px: u64,
    viewport_len_px: u64,
    offset_px: u64,
}

impl ScrollMech {
    /// Builds scroll state at offset zero.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError`] when the viewport is zero or exceeds
    /// [`MAX_VIEWPORT_PX`], or content exceeds [`MAX_SCROLL_CONTENT_PX`].
    pub fn new(
        node: UiNodeId,
        content_len_px: u64,
        viewport_len_px: u64,
    ) -> Result<Self, WidgetMechError> {
        check_viewport(viewport_len_px)?;
        if content_len_px > MAX_SCROLL_CONTENT_PX {
            return Err(WidgetMechError::BadScrollContent {
                found: content_len_px,
                cap: MAX_SCROLL_CONTENT_PX,
            });
        }
        Ok(Self {
            node,
            content_len_px,
            viewport_len_px,
            offset_px: 0,
        })
    }

    /// The tree node this mechanism drives.
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    /// Current settled offset in pixels (always `<= max_offset`).
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset_px
    }

    /// Largest valid offset: content minus viewport, floored at zero.
    #[must_use]
    pub fn max_offset(&self) -> u64 {
        self.content_len_px.saturating_sub(self.viewport_len_px)
    }

    /// Sets the offset, clamped to `0..=max_offset`.
    pub fn set_offset(&mut self, offset_px: u64) {
        self.offset_px = offset_px.min(self.max_offset());
    }

    /// Moves the offset by a signed delta, saturating at both ends and
    /// clamped to `max_offset`.
    pub fn scroll_by(&mut self, delta_px: i64) {
        let next = (self.offset_px as i64).saturating_add(delta_px).max(0) as u64;
        self.set_offset(next);
    }

    /// Whether the view sits at the start of the content.
    #[must_use]
    pub const fn is_at_top(&self) -> bool {
        self.offset_px == 0
    }

    /// Whether the view sits at the end of the content.
    #[must_use]
    pub fn is_at_bottom(&self) -> bool {
        self.offset_px >= self.max_offset()
    }
}

// ---------------------------------------------------------------------------
// Canvas mechanism: display-list budget (Rust), raster (elsewhere)
// ---------------------------------------------------------------------------

/// Rust-owned paint budget for one `Canvas` node.
///
/// Counts queued display-list commands against [`MAX_MECH_CANVAS_COMMANDS`].
/// Decode, rasterization, and pixels live outside this crate's UI role;
/// Lua defines what each command draws, Rust bounds how many may queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CanvasMech {
    node: UiNodeId,
    width_px: u32,
    height_px: u32,
    commands: u32,
}

impl CanvasMech {
    /// Builds canvas budget state with an empty command queue.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::BadCanvasSize`] when a dimension is zero
    /// or exceeds [`MAX_MECH_CANVAS_DIM_PX`].
    pub fn new(node: UiNodeId, width_px: u32, height_px: u32) -> Result<Self, WidgetMechError> {
        if width_px == 0
            || height_px == 0
            || width_px > MAX_MECH_CANVAS_DIM_PX
            || height_px > MAX_MECH_CANVAS_DIM_PX
        {
            return Err(WidgetMechError::BadCanvasSize {
                width: width_px,
                height: height_px,
            });
        }
        Ok(Self {
            node,
            width_px,
            height_px,
            commands: 0,
        })
    }

    /// The tree node this mechanism drives.
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    /// Canvas width in pixels.
    #[must_use]
    pub const fn width_px(&self) -> u32 {
        self.width_px
    }

    /// Canvas height in pixels.
    #[must_use]
    pub const fn height_px(&self) -> u32 {
        self.height_px
    }

    /// Queued display-list commands.
    #[must_use]
    pub const fn commands(&self) -> u32 {
        self.commands
    }

    /// Commands still queueable before the budget closes.
    #[must_use]
    pub fn budget_remaining(&self) -> u32 {
        MAX_MECH_CANVAS_COMMANDS.saturating_sub(self.commands)
    }

    /// Queues `count` commands against the budget.
    ///
    /// # Errors
    ///
    /// Returns [`WidgetMechError::CommandBudgetExceeded`] without mutating
    /// when the queue would exceed [`MAX_MECH_CANVAS_COMMANDS`].
    pub fn push_commands(&mut self, count: u32) -> Result<(), WidgetMechError> {
        let next = u64::from(self.commands).saturating_add(u64::from(count));
        if next > u64::from(MAX_MECH_CANVAS_COMMANDS) {
            return Err(WidgetMechError::CommandBudgetExceeded {
                requested: next,
                cap: u64::from(MAX_MECH_CANVAS_COMMANDS),
            });
        }
        self.commands = next as u32;
        Ok(())
    }

    /// Drops the queued commands, reopening the full budget.
    pub fn clear(&mut self) {
        self.commands = 0;
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> UiNodeId {
        UiNodeId::new(raw)
    }

    #[test]
    fn virtual_window_covers_partial_rows() {
        let mech = VirtualListMech::new(id(1), 100, 20, 55).expect("valid mech");
        assert_eq!(mech.content_height_px(), 2000);
        assert_eq!(mech.max_offset(), 1945);
        assert_eq!(mech.visible_range(), 0..3);
    }

    #[test]
    fn virtual_window_clamps_at_end() {
        let mut mech = VirtualListMech::new(id(1), 10, 20, 55).expect("valid mech");
        mech.set_offset(u64::MAX);
        assert_eq!(mech.offset(), mech.max_offset());
        assert_eq!(mech.visible_range(), 7..10);
    }

    #[test]
    fn virtual_empty_list_yields_empty_window() {
        let mech = VirtualListMech::new(id(1), 0, 20, 55).expect("valid mech");
        assert_eq!(mech.visible_range(), 0..0);
        assert_eq!(mech.item_offset(0), None);
    }

    #[test]
    fn virtual_bounds_fail_closed() {
        assert!(matches!(
            VirtualListMech::new(id(1), MAX_VIRTUAL_ITEMS + 1, 20, 55),
            Err(WidgetMechError::TooManyItems { .. })
        ));
        assert!(matches!(
            VirtualListMech::new(id(1), 10, 0, 55),
            Err(WidgetMechError::BadItemHeight { .. })
        ));
        assert!(matches!(
            VirtualListMech::new(id(1), 10, 20, 0),
            Err(WidgetMechError::BadViewport { .. })
        ));
    }

    #[test]
    fn virtual_ensure_visible_scrolls_minimally() {
        let mut mech = VirtualListMech::new(id(1), 100, 20, 60).expect("valid mech");
        mech.ensure_visible(10);
        assert_eq!(mech.offset(), 160);
        mech.ensure_visible(10);
        assert_eq!(mech.offset(), 160, "already visible rows do not move");
        mech.ensure_visible(0);
        assert_eq!(mech.offset(), 0);
    }

    #[test]
    fn text_input_edits_stay_char_based() {
        let mut mech = TextInputMech::new(id(2), "héllo".to_string()).expect("valid mech");
        assert_eq!(mech.cursor(), 5);
        mech.move_cursor(-2);
        mech.insert("X").expect("insert");
        assert_eq!(mech.value(), "hélXlo");
        assert!(mech.delete_before());
        assert_eq!(mech.value(), "héllo");
        mech.move_cursor(i32::MIN);
        assert_eq!(mech.cursor(), 0);
        assert!(!mech.delete_before());
    }

    #[test]
    fn text_input_ime_preedit_round_trip() {
        let mut mech = TextInputMech::new(id(2), String::new()).expect("valid mech");
        mech.set_preedit("ni".to_string()).expect("preedit");
        assert_eq!(mech.preedit(), "ni");
        assert_eq!(mech.value(), "");
        mech.commit_preedit().expect("commit");
        assert_eq!(mech.value(), "ni");
        assert_eq!(mech.preedit(), "");
        assert_eq!(mech.cursor(), 2);
    }

    #[test]
    fn text_input_overflow_fails_without_mutation() {
        let big = "x".repeat(MAX_UI_TEXT_LEN);
        let mut mech = TextInputMech::new(id(2), big.clone()).expect("valid mech");
        let err = mech.insert("y").expect_err("overflow must fail");
        assert_eq!(
            err,
            WidgetMechError::TextTooLong {
                len: MAX_UI_TEXT_LEN + 1,
                cap: MAX_UI_TEXT_LEN
            }
        );
        assert_eq!(mech.value(), big);
    }

    #[test]
    fn scroll_clamps_both_ends() {
        let mut mech = ScrollMech::new(id(3), 1000, 200).expect("valid mech");
        assert!(mech.is_at_top());
        mech.scroll_by(i64::MAX);
        assert_eq!(mech.offset(), 800);
        assert!(mech.is_at_bottom());
        mech.scroll_by(i64::MIN);
        assert_eq!(mech.offset(), 0);
        assert!(mech.is_at_top());
    }

    #[test]
    fn scroll_content_fits_reports_bottom() {
        let mech = ScrollMech::new(id(3), 100, 200).expect("valid mech");
        assert_eq!(mech.max_offset(), 0);
        assert!(mech.is_at_bottom());
    }

    #[test]
    fn canvas_budget_fails_closed_then_clears() {
        let mut mech = CanvasMech::new(id(4), 800, 600).expect("valid mech");
        mech.push_commands(MAX_MECH_CANVAS_COMMANDS)
            .expect("exact budget");
        assert_eq!(mech.budget_remaining(), 0);
        let err = mech
            .push_commands(1)
            .expect_err("over-budget push must fail");
        assert_eq!(
            err,
            WidgetMechError::CommandBudgetExceeded {
                requested: u64::from(MAX_MECH_CANVAS_COMMANDS) + 1,
                cap: u64::from(MAX_MECH_CANVAS_COMMANDS),
            }
        );
        assert_eq!(mech.commands(), MAX_MECH_CANVAS_COMMANDS);
        mech.clear();
        assert_eq!(mech.budget_remaining(), MAX_MECH_CANVAS_COMMANDS);
    }

    #[test]
    fn error_display_is_human_readable() {
        let err = WidgetMechError::BadItemHeight { found: 0 };
        assert_eq!(err.to_string(), "bad virtual row height: 0px");
    }
}
