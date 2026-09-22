//! Bounded `Canvas` display lists rendered by the compositor at refresh
//! (UX-25, CTX-0671).
//!
//! Candidate implementation of U-6 (`Canvas` half). Nothing here is
//! normative, accepted, or verified: the command set, every bound, and
//! every rule below is a candidate spelling that the UI Runtime RFC
//! accepts or rejects, never this module. The module is English-only.
//!
//! What this module provides:
//!
//! - [`CanvasCommand`] — the candidate retained command set (`fill-rect`,
//!   `stroke-rect`, `line`, `circle`, `text`). The tree carries the list,
//!   never pixels: decode, rasterization, and paint stay Rust-owned and
//!   run in the compositor at refresh. There is no per-frame Lua draw
//!   loop: Lua emits a display list on state change, Rust retains it, and
//!   the compositor replays the retained list while it is current
//!   (low-frequency, frame-on-demand).
//! - [`CanvasDisplayList`] — one bounded command sequence addressed to the
//!   `Canvas` node with [`UiNodeId`](crate::uitree::UiNodeId), the
//!   canonical identity (this module defines no id of its own).
//! - [`CanvasLayer`] — the retained per-scene map of display lists with a
//!   monotonic revision. [`CanvasLayer::submit`] reports whether the
//!   retained state changed so the compositor replays only on change,
//!   mirroring [`UiTree`](crate::uitree::UiTree) revision gating.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock
//! time, randomness, or platform handle participates.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::uitree::UiNodeId;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Hard cap on commands per display list.
///
/// Rejected with [`CanvasError::TooManyCommands`], never silently
/// truncated: truncation would present a partial drawing as complete.
pub const MAX_CANVAS_COMMANDS: usize = 256;

/// Hard cap on retained display lists per layer (one per `Canvas` node).
///
/// Rejected with [`CanvasError::TooManySurfaces`], never silently
/// evicted.
pub const MAX_CANVAS_SURFACES: usize = 64;

/// Hard cap in characters for `text` command payloads.
///
/// Rejected with [`CanvasError::TextTooLong`], never silently truncated.
pub const MAX_CANVAS_TEXT_LEN: usize = 512;

/// Absolute bound on every command coordinate (inclusive).
///
/// Rejected with [`CanvasError::CoordOutOfRange`]: unbounded coordinates
/// are an unbounded raster budget on the compositor.
pub const MAX_CANVAS_COORD: i32 = 16_384;

/// Hard cap on circle radius in device pixels (inclusive).
pub const MAX_CANVAS_RADIUS: i32 = 8_192;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build, extend, or retain a display list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanvasError {
    /// The list holds more than [`MAX_CANVAS_COMMANDS`] commands.
    TooManyCommands {
        /// Commands counted in the rejected submission.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// The layer holds more than [`MAX_CANVAS_SURFACES`] display lists.
    TooManySurfaces {
        /// Surfaces counted in the layer.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A `text` payload exceeds [`MAX_CANVAS_TEXT_LEN`] characters.
    TextTooLong {
        /// Length in characters of the rejected payload.
        len: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A rectangle has a non-positive side.
    EmptyRect {
        /// Rejected width.
        w: i32,
        /// Rejected height.
        h: i32,
    },
    /// A circle has a non-positive or oversized radius.
    BadRadius {
        /// Rejected radius.
        r: i32,
        /// The cap that was exceeded.
        cap: i32,
    },
    /// A coordinate exceeds [`MAX_CANVAS_COORD`] in absolute value.
    CoordOutOfRange {
        /// Rejected coordinate value.
        value: i32,
        /// The bound that was exceeded.
        bound: i32,
    },
}

impl fmt::Display for CanvasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyCommands { found, cap } => {
                write!(
                    f,
                    "canvas display list too long: {found} commands exceeds cap {cap}"
                )
            }
            Self::TooManySurfaces { found, cap } => {
                write!(f, "canvas layer full: {found} surfaces exceeds cap {cap}")
            }
            Self::TextTooLong { len, cap } => {
                write!(f, "canvas text too long: {len} chars exceeds cap {cap}")
            }
            Self::EmptyRect { w, h } => {
                write!(f, "canvas rect is empty: w={w} h={h}")
            }
            Self::BadRadius { r, cap } => {
                write!(f, "canvas radius out of range: {r} exceeds cap {cap}")
            }
            Self::CoordOutOfRange { value, bound } => {
                write!(
                    f,
                    "canvas coordinate out of range: {value} exceeds bound {bound}"
                )
            }
        }
    }
}

impl std::error::Error for CanvasError {}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// One retained drawing command (candidate U-6 command set).
///
/// Coordinates are integer device pixels in the `Canvas` node's local
/// space. Colors, fonts, and stroke widths are appearance policy owned by
/// a later RFC: commands carry geometry and text only, so no half-chosen
/// style vocabulary lands here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanvasCommand {
    /// Filled axis-aligned rectangle at (`x`, `y`) with size (`w`, `h`).
    FillRect {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width in pixels (must be positive).
        w: i32,
        /// Height in pixels (must be positive).
        h: i32,
    },
    /// Stroked axis-aligned rectangle outline.
    StrokeRect {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Width in pixels (must be positive).
        w: i32,
        /// Height in pixels (must be positive).
        h: i32,
    },
    /// Straight segment from (`x0`, `y0`) to (`x1`, `y1`).
    Line {
        /// Start x.
        x0: i32,
        /// Start y.
        y0: i32,
        /// End x.
        x1: i32,
        /// End y.
        y1: i32,
    },
    /// Stroked circle centered at (`cx`, `cy`).
    Circle {
        /// Center x.
        cx: i32,
        /// Center y.
        cy: i32,
        /// Radius in pixels (must be within `1..=MAX_CANVAS_RADIUS`).
        r: i32,
    },
    /// Text run with its top-left corner at (`x`, `y`).
    Text {
        /// Left edge.
        x: i32,
        /// Top edge.
        y: i32,
        /// Run body (bounded by [`MAX_CANVAS_TEXT_LEN`]).
        body: String,
    },
}

impl CanvasCommand {
    /// Candidate vocabulary spelling for this command.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FillRect { .. } => "fill-rect",
            Self::StrokeRect { .. } => "stroke-rect",
            Self::Line { .. } => "line",
            Self::Circle { .. } => "circle",
            Self::Text { .. } => "text",
        }
    }

    /// Validates bounds for a single command.
    ///
    /// # Errors
    ///
    /// Returns [`CanvasError`] when a coordinate, size, radius, or text
    /// payload violates its bound.
    pub fn validate(&self) -> Result<(), CanvasError> {
        match self {
            Self::FillRect { x, y, w, h } | Self::StrokeRect { x, y, w, h } => {
                check_coord(*x)?;
                check_coord(*y)?;
                check_coord(*w)?;
                check_coord(*h)?;
                if *w <= 0 || *h <= 0 {
                    return Err(CanvasError::EmptyRect { w: *w, h: *h });
                }
                Ok(())
            }
            Self::Line { x0, y0, x1, y1 } => {
                check_coord(*x0)?;
                check_coord(*y0)?;
                check_coord(*x1)?;
                check_coord(*y1)?;
                Ok(())
            }
            Self::Circle { cx, cy, r } => {
                check_coord(*cx)?;
                check_coord(*cy)?;
                if *r <= 0 || *r > MAX_CANVAS_RADIUS {
                    return Err(CanvasError::BadRadius {
                        r: *r,
                        cap: MAX_CANVAS_RADIUS,
                    });
                }
                Ok(())
            }
            Self::Text { x, y, body } => {
                check_coord(*x)?;
                check_coord(*y)?;
                let len = body.chars().count();
                if len > MAX_CANVAS_TEXT_LEN {
                    return Err(CanvasError::TextTooLong {
                        len,
                        cap: MAX_CANVAS_TEXT_LEN,
                    });
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for CanvasCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn check_coord(value: i32) -> Result<(), CanvasError> {
    if !(-MAX_CANVAS_COORD..=MAX_CANVAS_COORD).contains(&value) {
        return Err(CanvasError::CoordOutOfRange {
            value,
            bound: MAX_CANVAS_COORD,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Display list
// ---------------------------------------------------------------------------

/// Bounded command sequence for one `Canvas` node.
///
/// Addressed by [`UiNodeId`](crate::uitree::UiNodeId): the list never
/// carries pixels, only the retained commands the compositor replays at
/// refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasDisplayList {
    node: UiNodeId,
    commands: Vec<CanvasCommand>,
}

impl CanvasDisplayList {
    /// Builds an empty list for `node`.
    #[must_use]
    pub fn new(node: UiNodeId) -> Self {
        Self {
            node,
            commands: Vec::new(),
        }
    }

    /// The `Canvas` node this list draws into.
    #[must_use]
    pub const fn node(&self) -> UiNodeId {
        self.node
    }

    /// Retained commands in replay order.
    #[must_use]
    pub fn commands(&self) -> &[CanvasCommand] {
        &self.commands
    }

    /// Number of retained commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether the list holds no commands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Appends a validated command.
    ///
    /// # Errors
    ///
    /// Returns [`CanvasError`] when the command is invalid or the list is
    /// already at [`MAX_CANVAS_COMMANDS`]; the retained list is unchanged.
    pub fn push(&mut self, command: CanvasCommand) -> Result<(), CanvasError> {
        command.validate()?;
        if self.commands.len() >= MAX_CANVAS_COMMANDS {
            return Err(CanvasError::TooManyCommands {
                found: self.commands.len().saturating_add(1),
                cap: MAX_CANVAS_COMMANDS,
            });
        }
        self.commands.push(command);
        Ok(())
    }

    /// Drops all retained commands (keeps the node address).
    pub fn clear(&mut self) {
        self.commands.clear();
    }
}

// ---------------------------------------------------------------------------
// Retained layer
// ---------------------------------------------------------------------------

/// Report of [`CanvasLayer::submit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasSubmitReport {
    /// Revision after the call (bumped only when `changed`).
    pub revision: u64,
    /// Whether the retained list for the node differs.
    pub changed: bool,
}

/// Retained per-scene map of display lists with a monotonic revision.
///
/// The revision starts at `0` and bumps by one per accepted change, so
/// the compositor replays lists at refresh only while the revision
/// moved (low-frequency replay, never a per-frame Lua callback).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanvasLayer {
    lists: BTreeMap<UiNodeId, CanvasDisplayList>,
    revision: u64,
}

impl CanvasLayer {
    /// Builds an empty layer at revision `0`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lists: BTreeMap::new(),
            revision: 0,
        }
    }

    /// Current revision (starts at `0`).
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Number of retained display lists.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lists.len()
    }

    /// Whether no display list is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lists.is_empty()
    }

    /// Retained list for `node`, if any.
    #[must_use]
    pub fn get(&self, node: UiNodeId) -> Option<&CanvasDisplayList> {
        self.lists.get(&node)
    }

    /// Submits a display list for its node.
    ///
    /// Every command is validated first: a rejected list never touches
    /// the retained state. An identical resubmission returns
    /// `changed: false` with the revision untouched; a different list
    /// replaces the retained one and bumps the revision by one.
    ///
    /// # Errors
    ///
    /// Returns [`CanvasError`] for an invalid list, or
    /// [`CanvasError::TooManySurfaces`] when the list addresses a new
    /// node while the layer is full; the retained state is unchanged.
    pub fn submit(&mut self, list: CanvasDisplayList) -> Result<CanvasSubmitReport, CanvasError> {
        for command in &list.commands {
            command.validate()?;
        }
        if list.commands.len() > MAX_CANVAS_COMMANDS {
            return Err(CanvasError::TooManyCommands {
                found: list.commands.len(),
                cap: MAX_CANVAS_COMMANDS,
            });
        }
        if !self.lists.contains_key(&list.node) && self.lists.len() >= MAX_CANVAS_SURFACES {
            return Err(CanvasError::TooManySurfaces {
                found: self.lists.len().saturating_add(1),
                cap: MAX_CANVAS_SURFACES,
            });
        }
        let changed = self.lists.get(&list.node) != Some(&list);
        if !changed {
            return Ok(CanvasSubmitReport {
                revision: self.revision,
                changed: false,
            });
        }
        self.lists.insert(list.node, list);
        self.revision = self.revision.wrapping_add(1);
        Ok(CanvasSubmitReport {
            revision: self.revision,
            changed: true,
        })
    }

    /// Retires the display list for `node`.
    ///
    /// Returns `true` and bumps the revision when a list was retained;
    /// a missing node is a silent no-op with the revision untouched.
    pub fn remove(&mut self, node: UiNodeId) -> bool {
        if self.lists.remove(&node).is_none() {
            return false;
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }
}

impl Default for CanvasLayer {
    /// Empty layer at revision `0`.
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn list(node: u64) -> CanvasDisplayList {
        CanvasDisplayList::new(UiNodeId::new(node))
    }

    #[test]
    fn push_accepts_valid_commands_in_order() {
        let mut display = list(1);
        display
            .push(CanvasCommand::FillRect {
                x: 0,
                y: 0,
                w: 8,
                h: 4,
            })
            .expect("valid rect");
        display
            .push(CanvasCommand::Text {
                x: 1,
                y: 1,
                body: "hi".to_string(),
            })
            .expect("valid text");
        assert_eq!(display.len(), 2);
        assert_eq!(display.commands()[0].as_str(), "fill-rect");
        assert_eq!(display.commands()[1].as_str(), "text");
    }

    #[test]
    fn push_caps_commands_fail_closed() {
        let mut display = list(1);
        for _ in 0..MAX_CANVAS_COMMANDS {
            display
                .push(CanvasCommand::Line {
                    x0: 0,
                    y0: 0,
                    x1: 1,
                    y1: 1,
                })
                .expect("room remains");
        }
        let err = display
            .push(CanvasCommand::Line {
                x0: 0,
                y0: 0,
                x1: 1,
                y1: 1,
            })
            .expect_err("overflow must fail");
        assert_eq!(
            err,
            CanvasError::TooManyCommands {
                found: MAX_CANVAS_COMMANDS + 1,
                cap: MAX_CANVAS_COMMANDS
            }
        );
        assert_eq!(display.len(), MAX_CANVAS_COMMANDS);
    }

    #[test]
    fn empty_rect_and_bad_radius_fail_closed() {
        let mut display = list(1);
        let err = display
            .push(CanvasCommand::FillRect {
                x: 0,
                y: 0,
                w: 0,
                h: 4,
            })
            .expect_err("empty rect must fail");
        assert_eq!(err, CanvasError::EmptyRect { w: 0, h: 4 });
        let err = display
            .push(CanvasCommand::Circle { cx: 0, cy: 0, r: 0 })
            .expect_err("zero radius must fail");
        assert_eq!(
            err,
            CanvasError::BadRadius {
                r: 0,
                cap: MAX_CANVAS_RADIUS
            }
        );
        assert!(display.is_empty());
    }

    #[test]
    fn out_of_range_coord_fails_closed() {
        let err = CanvasCommand::Line {
            x0: MAX_CANVAS_COORD + 1,
            y0: 0,
            x1: 0,
            y1: 0,
        }
        .validate()
        .expect_err("oversized coord must fail");
        assert_eq!(
            err,
            CanvasError::CoordOutOfRange {
                value: MAX_CANVAS_COORD + 1,
                bound: MAX_CANVAS_COORD
            }
        );
    }

    #[test]
    fn oversized_text_fails_closed() {
        let big = "x".repeat(MAX_CANVAS_TEXT_LEN + 1);
        let err = CanvasCommand::Text {
            x: 0,
            y: 0,
            body: big,
        }
        .validate()
        .expect_err("oversized text must fail");
        assert_eq!(
            err,
            CanvasError::TextTooLong {
                len: MAX_CANVAS_TEXT_LEN + 1,
                cap: MAX_CANVAS_TEXT_LEN
            }
        );
    }

    #[test]
    fn identical_resubmission_does_not_bump_revision() {
        let mut layer = CanvasLayer::new();
        let mut first = list(1);
        first
            .push(CanvasCommand::Circle { cx: 2, cy: 2, r: 3 })
            .expect("valid circle");
        let report = layer.submit(first.clone()).expect("valid list");
        assert!(report.changed);
        assert_eq!(report.revision, 1);
        let report = layer.submit(first).expect("valid list");
        assert!(!report.changed);
        assert_eq!(report.revision, 1);
        assert_eq!(layer.revision(), 1);
    }

    #[test]
    fn rejected_submit_keeps_retained_state() {
        let mut layer = CanvasLayer::new();
        let mut kept = list(1);
        kept.push(CanvasCommand::Line {
            x0: 0,
            y0: 0,
            x1: 1,
            y1: 1,
        })
        .expect("valid line");
        layer.submit(kept).expect("valid list");
        let mut bad = list(2);
        // Bypass `push` validation to exercise `submit` validation.
        bad.commands.push(CanvasCommand::FillRect {
            x: 0,
            y: 0,
            w: -1,
            h: 2,
        });
        let err = layer.submit(bad).expect_err("invalid list must fail");
        assert_eq!(err, CanvasError::EmptyRect { w: -1, h: 2 });
        assert_eq!(layer.len(), 1);
        assert_eq!(layer.revision(), 1);
        assert!(layer.get(UiNodeId::new(2)).is_none());
    }

    #[test]
    fn layer_caps_surfaces_and_remove_bumps_revision() {
        let mut layer = CanvasLayer::new();
        for raw in 0..MAX_CANVAS_SURFACES as u64 {
            layer.submit(list(raw)).expect("room remains");
        }
        let err = layer
            .submit(list(MAX_CANVAS_SURFACES as u64))
            .expect_err("overflow must fail");
        assert_eq!(
            err,
            CanvasError::TooManySurfaces {
                found: MAX_CANVAS_SURFACES + 1,
                cap: MAX_CANVAS_SURFACES
            }
        );
        let before = layer.revision();
        assert!(layer.remove(UiNodeId::new(0)));
        assert_eq!(layer.revision(), before.wrapping_add(1));
        assert!(!layer.remove(UiNodeId::new(0)));
        assert_eq!(layer.revision(), before.wrapping_add(1));
    }

    #[test]
    fn error_display_is_human_readable() {
        assert_eq!(
            CanvasError::TooManyCommands {
                found: 257,
                cap: 256
            }
            .to_string(),
            "canvas display list too long: 257 commands exceeds cap 256"
        );
    }
}
