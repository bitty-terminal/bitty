//! `Runtime` — Overlay scrollbar presentation and drag interaction.
//!
//! Split of the CTX-0181 slice: the overlay scrollbar thumb is painted in
//! the present layer (like the selection highlight and the paste banner)
//! and never touches grid truth, scrollback, or layout. Geometry comes from
//! [`bitty_ui::scrollbar`] (pure, headless); this module only resolves the
//! focused leaf's track/thumb in window pixels, gates visibility by mode,
//! and routes press/move/release through the existing [`View::scroll_by`]
//! (no scroll-semantics change).
//!
//! Lane note: scrollbar chrome only. Wheel/page scrolling, scrollback
//! retention, and viewport compositing belong to their owning paths.

use super::*;
use bitty_ui::scrollbar::{ThumbSpan, TrackRect};

/// Active overlay-thumb drag: which view scrolls plus the press-time grab
/// offset (cursor y minus thumb top, in track space).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollbarDrag {
    /// View scrolled by this drag (the focused leaf at press time).
    pub view: ViewId,
    /// Cursor distance below the thumb top when grabbed (physical px).
    pub grab_offset_px: f64,
}

impl Runtime {
    /// Retained scrollback lines backing the scrollbar thumb.
    ///
    /// Read-only headless seam (mirrors [`Self::snapshot`]): the focused
    /// pane's session length when splits own shells, else the primary
    /// state's length.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        if let Some(fid) = self.focused_view() {
            if let Some(sess) = self.pane_sessions.get(&fid) {
                return sess.state.scrollback_len();
            }
        }
        self.state.scrollback_len()
    }

    /// Configured thumb width in physical pixels at the live DPI scale.
    ///
    /// The logical width ([`crate::config::RuntimeConfig::scrollbar_width`])
    /// scaled by the sanitized live factor, rounded half away from zero —
    /// the same rule as [`Self::window_padding_physical`] so chrome and
    /// padding agree about the pixel grid. Validated `>= 1` logical, so a
    /// zero result only comes from a degenerate scale (caller paints
    /// nothing then).
    #[must_use]
    pub fn scrollbar_width_physical(&self) -> u32 {
        let scaled = f64::from(
            self.config
                .scrollbar_width
                .min(crate::config::MAX_SCROLLBAR_WIDTH_PX),
        ) * sanitize_dpi_scale(self.dpi_scale());
        let rounded = scaled.round();
        if rounded < 1.0 || rounded >= f64::from(u32::MAX) {
            0
        } else {
            rounded as u32
        }
    }

    /// Resolves the focused leaf's scrollbar track and thumb in window
    /// physical pixels, or `None` when there is nothing to scroll or the
    /// geometry is degenerate.
    ///
    /// CTX-0313: the track hugs the focused leaf's decorated content frame
    /// ([`Self::present_frames`], the same rectangle the present layer
    /// paints) instead of the raw CTX-0177 cell allocation, so the thumb
    /// never covers the decoration gap/border bands; the physical padding
    /// inset (CTX-0223) is added exactly like the present layer's content
    /// translation. No mode gate here — callers apply
    /// [`bitty_ui::scrollbar::is_visible`] with their engagement signal.
    pub(super) fn scrollbar_track_thumb(&self) -> Option<(ViewId, TrackRect, ThumbSpan)> {
        use bitty_ui::scrollbar::{TrackSpec, thumb_geometry, track_rect};
        let width = self.scrollbar_width_physical();
        if width == 0 {
            return None;
        }
        let frames = self.present_frames();
        let fid = self
            .focused_view()
            .or_else(|| frames.first().map(|frame| frame.view))?;
        let frame = frames.iter().find(|frame| frame.view == fid)?;
        let view = self.layout.find_leaf(fid)?;
        let sb_len = self.scrollback_len();
        let track = track_rect(TrackSpec {
            content_x: frame.content.x,
            content_y: frame.content.y,
            content_w: frame.content.width,
            content_h: frame.content.height,
            pad_px: self.window_padding_physical(),
            width_px: width,
        })?;
        let thumb = thumb_geometry(
            track.height,
            usize::from(view.rows()),
            sb_len,
            view.scroll_offset(),
        )?;
        Some((fid, track, thumb))
    }

    /// Whether the cursor currently engages the scrollbar (hover, proximity,
    /// or active drag) — the `auto` visibility signal.
    ///
    /// Pure over `&self`: an active drag always engages; otherwise the last
    /// known cursor position (the existing mouse path's `last_cursor`) must
    /// fall in the track's proximity zone. A cursor that never entered the
    /// window — or that left it (`CursorLeft`, tracked separately so the
    /// shared `last_cursor` keeps its selection-path meaning) — engages
    /// nothing.
    pub(super) fn scrollbar_engaged(&self) -> bool {
        if self.scrollbar_drag.is_some() {
            return true;
        }
        if self.scrollbar_cursor_left {
            return false;
        }
        let Some((_, track, _)) = self.scrollbar_track_thumb() else {
            return false;
        };
        match self.last_cursor {
            Some(pos) => track.near(pos.x, pos.y),
            None => false,
        }
    }

    /// Whether the scrollbar paints on the next present.
    ///
    /// The shipped default `auto` paints only while engaged (transparent at
    /// rest, so geometry-neutral); `hidden` never paints; `always` paints
    /// whenever scrollback exists. Content presence is implied by a resolved
    /// thumb.
    pub(super) fn scrollbar_should_paint(&self) -> bool {
        let Some(_) = self.scrollbar_track_thumb() else {
            return false;
        };
        bitty_ui::scrollbar::is_visible(self.config.scrollbar_mode, true, self.scrollbar_engaged())
    }

    /// Resolves the scrollbar thumb overlay fill for the focused leaf, if it
    /// paints now. Pure over `&self`: the present layer pushes the fill and
    /// records visibility; grid truth is never touched.
    ///
    /// The hue reuses the theme foreground at a translucent alpha — the same
    /// pattern as the cursor overlay (theme cursor hue plus alpha
    /// override) — so the thumb stays legible on any grid content without
    /// introducing a new palette constant.
    pub(super) fn scrollbar_thumb_fill(&self) -> Option<bitty_render::grid::FillRect> {
        if !self.scrollbar_should_paint() {
            return None;
        }
        let (_, track, thumb) = self.scrollbar_track_thumb()?;
        // CTX-0355: the thumb follows the resolved theme foreground.
        let mut color = self.config.theme.foreground;
        color[3] = 0x99;
        Some(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                track.x,
                track.y.saturating_add(thumb.y.min(i32::MAX as u32) as i32),
                track.width,
                thumb.height,
            ),
            color,
        })
    }

    /// Current scrollbar track in window physical pixels, if any.
    ///
    /// Headless seam for embedders and tests: `None` when there is nothing
    /// to scroll or the geometry is degenerate (same resolution the present
    /// layer paints from — no mode gate, so callers can probe geometry
    /// independent of visibility).
    #[must_use]
    pub fn scrollbar_track(&self) -> Option<bitty_ui::scrollbar::TrackRect> {
        self.scrollbar_track_thumb().map(|(_, track, _)| track)
    }

    /// Whether `pos` hits the currently painted scrollbar thumb/track.
    ///
    /// Headless seam for embedders: a release over the thumb ends a scroll
    /// gesture and must not activate hyperlinks beneath the mapped cell.
    /// `false` whenever nothing paints (hidden mode, empty history,
    /// auto-hide idle) or the position falls outside the track.
    #[must_use]
    pub fn scrollbar_hit_at(&self, pos: CursorPosition) -> bool {
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return false;
        }
        if !self.scrollbar_should_paint() {
            return false;
        }
        let Some((_, track, thumb)) = self.scrollbar_track_thumb() else {
            return false;
        };
        bitty_ui::scrollbar::hit_test(track, thumb, pos.x, pos.y).is_some()
    }

    /// Whether a scrollbar thumb drag is in progress (headless seam).
    #[must_use]
    pub fn is_scrollbar_dragging(&self) -> bool {
        self.scrollbar_drag.is_some()
    }

    /// Whether the scrollbar painted on the last present (headless seam).
    ///
    /// Tracks the last presented frame so `auto` hover/proximity transitions
    /// are observable without screenshots; updated on every tick that
    /// presents (idle ticks leave it untouched, like the frame itself).
    #[must_use]
    pub fn scrollbar_is_visible(&self) -> bool {
        self.scrollbar_visible
    }

    /// Attempts to start a scrollbar drag for a left press at the last known
    /// cursor position. Returns `true` when consumed (the selection/PTY
    /// path must skip the event).
    ///
    /// Starts only on the painted thumb/track: a press where no thumb
    /// paints (hidden mode, empty history, auto-hide idle, or outside the
    /// track) returns `false` and selection keeps the event. The grab offset
    /// preserves the cursor's position within the thumb so the thumb does
    /// not jump on grab.
    pub(super) fn scrollbar_press(&mut self) -> bool {
        let Some(pos) = self.last_cursor else {
            return false;
        };
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return false;
        }
        let Some((view, track, thumb)) = self.scrollbar_track_thumb() else {
            return false;
        };
        if !self.scrollbar_should_paint() {
            return false;
        }
        let hit = bitty_ui::scrollbar::hit_test(track, thumb, pos.x, pos.y);
        if hit.is_none() {
            return false;
        }
        let grab =
            (pos.y - f64::from(track.y) - f64::from(thumb.y)).clamp(0.0, f64::from(thumb.height));
        self.scrollbar_drag = Some(ScrollbarDrag {
            view,
            grab_offset_px: grab,
        });
        self.pending_full_redraw = true;
        true
    }

    /// Continues an active scrollbar drag at `pos`: maps the cursor (minus
    /// the grab offset) back to a scroll offset through the existing
    /// [`View::scroll_by`] delta path. Returns `true` while a drag is
    /// active. Ends the drag (returns `false`) when the dragged view or its
    /// scrollback vanished mid-gesture.
    pub(super) fn scrollbar_drag_to(&mut self, pos: CursorPosition) -> bool {
        let Some(drag) = self.scrollbar_drag else {
            return false;
        };
        let Some((view, track, thumb)) = self.scrollbar_track_thumb() else {
            self.scrollbar_drag = None;
            self.pending_full_redraw = true;
            return false;
        };
        if view != drag.view {
            self.scrollbar_drag = None;
            self.pending_full_redraw = true;
            return false;
        }
        let sb_len = self.scrollback_len();
        let thumb_y = pos.y - f64::from(track.y) - drag.grab_offset_px;
        let offset =
            bitty_ui::scrollbar::offset_for_thumb_y(thumb_y, track.height, thumb.height, sb_len);
        if let Some(v) = self.layout.find_leaf_mut(view) {
            let cur = v.scroll_offset();
            v.scroll_by(offset as isize - cur as isize, sb_len);
        }
        self.pending_full_redraw = true;
        true
    }

    /// Ends an active scrollbar drag (release / cursor left). Returns `true`
    /// when a drag was active.
    pub(super) fn scrollbar_release(&mut self) -> bool {
        if self.scrollbar_drag.is_none() {
            return false;
        }
        self.scrollbar_drag = None;
        self.pending_full_redraw = true;
        true
    }
}
