//! Overlay scrollbar geometry (CTX-0181).
//!
//! Pure, headless presentation math for the scrollback scrollbar: thumb
//! sizing/positioning from scrollback length plus viewport offset, mode
//! gating (`always`/`auto`/`hidden`), track/thumb hit-testing, and
//! drag-to-offset mapping. No render, platform, or terminal-state coupling:
//! every function is total over its inputs (saturating arithmetic, clamped
//! outputs, never panics), so the runtime present layer and mouse path can
//! call these without any failure mode beyond "do not paint".
//!
//! # Overlay contract
//!
//! The scrollbar is presentation-only, like the selection highlight and the
//! paste banner: the runtime paints a [`ThumbSpan`] as `FillRect`s in the
//! present layer and never touches grid truth, scrollback, or layout. The
//! track lives **inside** the focused leaf's decorated content frame (right
//! edge), so enabling or hiding the scrollbar never changes grid geometry —
//! `auto` (the default since CTX-0362) adds zero fills and zero layout delta
//! at rest and `hidden` never paints at all.
//!
//! # Coordinate model
//!
//! All pixel values are physical pixels at the live DPI scale. The track is
//! resolved from the **decorated content frame** — the CTX-0294 present
//! rectangle inside the decoration border — plus the window padding inset
//! (CTX-0223): the caller passes the painted content rectangle, and
//! positions over the decoration gap/border bands or the padding band never
//! resolve into the track.
//!
//! # Scroll direction
//!
//! `scroll_offset == 0` is live (bottom): the thumb rests at the track
//! bottom. `scroll_offset == scrollback_len` is the oldest history (top):
//! the thumb rests at the track top. Dragging the thumb up scrolls into
//! history; dragging down returns toward live.

/// Scrollbar display mode (`scrollbar.mode`).
///
/// - [`ScrollbarMode::Auto`] (default since CTX-0362): painted only while
///   engaged — the cursor hovers the track/thumb, sits within
///   [`SCROLLBAR_PROXIMITY_PX`] of the track, or a thumb drag is active —
///   modern-terminal auto-hide behavior, transparent at rest.
/// - [`ScrollbarMode::Hidden`]: never painted, zero pixels, zero geometry
///   delta. Explicit opt-out.
/// - [`ScrollbarMode::Always`]: painted whenever there is scrollback to
///   scroll (`scrollback_len > 0`); hidden while history is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollbarMode {
    /// Never painted (opt-out; geometry-neutral).
    Hidden,
    /// Painted whenever scrollback exists.
    Always,
    /// Painted only while engaged (hover/proximity/drag). Default.
    #[default]
    Auto,
}

impl ScrollbarMode {
    /// Parses a config `mode` string (exact lowercase; fail-closed).
    ///
    /// Returns `None` for anything but `"hidden"`, `"always"`, `"auto"`
    /// (callers surface the field path; the value itself is never echoed).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "hidden" => Some(Self::Hidden),
            "always" => Some(Self::Always),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Canonical config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Always => "always",
            Self::Auto => "auto",
        }
    }
}

/// Minimum thumb height in physical pixels.
///
/// Huge scrollbacks shrink the proportional thumb below grabbable size; the
/// clamp keeps the thumb hittable. Position math accounts for the clamp so
/// the thumb still reaches both track ends exactly.
pub const MIN_THUMB_HEIGHT_PX: u32 = 12;

/// Proximity radius in physical pixels around the track that reveals an
/// `auto` scrollbar (cursor within this distance left of/above/below the
/// track, or over it, counts as engaged). Fixed v1 constant — deliberately
/// not configured (the task's config surface is `{ mode, width }`).
pub const SCROLLBAR_PROXIMITY_PX: u32 = 16;

/// Thumb span within its track: top offset and height, both physical px.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbSpan {
    /// Thumb top relative to the track top, in physical pixels.
    pub y: u32,
    /// Thumb height in physical pixels (`>= MIN_THUMB_HEIGHT_PX`).
    pub height: u32,
}

/// Track rectangle in physical pixels (window space, padding included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackRect {
    /// Left edge in physical pixels (window space).
    pub x: i32,
    /// Top edge in physical pixels (window space).
    pub y: i32,
    /// Track width in physical pixels (the configured thumb width).
    pub width: u32,
    /// Track height in physical pixels (the full leaf height).
    pub height: u32,
}

impl TrackRect {
    /// Whether a physical cursor position lies inside the track.
    #[must_use]
    pub fn contains(self, cursor_x: f64, cursor_y: f64) -> bool {
        if !cursor_x.is_finite() || !cursor_y.is_finite() {
            return false;
        }
        let x0 = f64::from(self.x);
        let y0 = f64::from(self.y);
        cursor_x >= x0
            && cursor_x < x0 + f64::from(self.width)
            && cursor_y >= y0
            && cursor_y < y0 + f64::from(self.height)
    }

    /// Whether a physical cursor position lies within the proximity radius
    /// of the track (engagement zone for `auto` mode).
    #[must_use]
    pub fn near(self, cursor_x: f64, cursor_y: f64) -> bool {
        if !cursor_x.is_finite() || !cursor_y.is_finite() {
            return false;
        }
        let pad = f64::from(SCROLLBAR_PROXIMITY_PX);
        let x0 = f64::from(self.x) - pad;
        let y0 = f64::from(self.y) - pad;
        let x1 = f64::from(self.x) + f64::from(self.width) + pad;
        let y1 = f64::from(self.y) + f64::from(self.height) + pad;
        cursor_x >= x0 && cursor_x < x1 && cursor_y >= y0 && cursor_y < y1
    }
}

/// Inputs resolving an overlay track: the decorated content frame in
/// physical pixels plus the padding inset. One struct keeps the resolver
/// total without arity lint pressure and keeps the frame/padding offsets
/// explicit at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSpec {
    /// Decorated content frame origin x in window physical pixels (CTX-0294
    /// present frame, before the window-padding inset).
    pub content_x: i32,
    /// Decorated content frame origin y in window physical pixels.
    pub content_y: i32,
    /// Decorated content frame width in physical pixels.
    pub content_w: u32,
    /// Decorated content frame height in physical pixels.
    pub content_h: u32,
    /// Window padding inset in physical pixels (CTX-0223).
    pub pad_px: u32,
    /// Thumb width in physical pixels.
    pub width_px: u32,
}

/// Computes the overlay track rectangle for a decorated content frame.
///
/// CTX-0313: the input is the *painted* content frame — the CTX-0294
/// present rectangle inside the decoration border — so the track hugs the
/// content edge instead of the raw cell allocation and never covers the
/// decoration gap/border bands. The physical window padding (CTX-0223) is
/// added by the resolver, exactly like the present layer's content
/// translation. The track is `width_px` wide and spans the full content
/// height.
///
/// Returns `None` for degenerate inputs (zero-size frame, zero thumb width,
/// thumb wider than the content): the caller paints nothing.
#[must_use]
pub fn track_rect(spec: TrackSpec) -> Option<TrackRect> {
    let TrackSpec {
        content_x,
        content_y,
        content_w,
        content_h,
        pad_px,
        width_px,
    } = spec;
    if content_w == 0 || content_h == 0 || width_px == 0 || content_w < width_px {
        return None;
    }
    let x = i64::from(content_x)
        .saturating_add(i64::from(pad_px))
        .saturating_add(i64::from(content_w))
        .saturating_sub(i64::from(width_px));
    let y = i64::from(content_y).saturating_add(i64::from(pad_px));
    if x < 0 || y < 0 {
        return None;
    }
    Some(TrackRect {
        x: x.min(i64::from(i32::MAX)) as i32,
        y: y.min(i64::from(i32::MAX)) as i32,
        width: width_px,
        height: content_h,
    })
}

/// Computes the thumb span for a track from scrollback geometry.
///
/// - `track_height_px`: full track height (from [`track_rect`]).
/// - `rows`: viewport rows; `scrollback_len`: retained history lines;
///   `scroll_offset`: lines up into history (`0` = live bottom), clamped to
///   `scrollback_len`.
///
/// Returns `None` when there is nothing to scroll (`scrollback_len == 0`)
/// or for degenerate inputs — the caller paints nothing. Otherwise the
/// thumb height is proportional (`rows / (rows + scrollback_len)`) clamped
/// to at least [`MIN_THUMB_HEIGHT_PX`], and the thumb top interpolates
/// linearly: bottom at live, top at oldest history.
#[must_use]
pub fn thumb_geometry(
    track_height_px: u32,
    rows: usize,
    scrollback_len: usize,
    scroll_offset: usize,
) -> Option<ThumbSpan> {
    if track_height_px == 0 || rows == 0 || scrollback_len == 0 {
        return None;
    }
    let track = u64::from(track_height_px);
    let total = (rows as u64).saturating_add(scrollback_len as u64);
    if total == 0 {
        return None;
    }
    let proportional = track.saturating_mul(rows as u64).checked_div(total)?;
    let height = proportional.max(u64::from(MIN_THUMB_HEIGHT_PX)).min(track);
    let travel = track.saturating_sub(height);
    let offset = (scroll_offset as u64).min(scrollback_len as u64);
    let remaining = (scrollback_len as u64).saturating_sub(offset);
    let y = travel
        .saturating_mul(remaining)
        .checked_div(scrollback_len as u64)
        .unwrap_or(0);
    Some(ThumbSpan {
        y: y.min(u64::from(u32::MAX)) as u32,
        height: height.min(u64::from(u32::MAX)) as u32,
    })
}

/// Mode gate: whether a scrollbar with scrollback content may paint.
///
/// - `Hidden` never paints (zero-pixel, geometry-neutral by construction).
/// - `Always` paints whenever `has_content` (scrollback exists).
/// - `Auto` paints only while `engaged` (hover, proximity, or active drag).
#[must_use]
pub const fn is_visible(mode: ScrollbarMode, has_content: bool, engaged: bool) -> bool {
    if !has_content {
        return false;
    }
    match mode {
        ScrollbarMode::Hidden => false,
        ScrollbarMode::Always => true,
        ScrollbarMode::Auto => engaged,
    }
}

/// Hit-test result for a press inside the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarHit {
    /// On the thumb: starts a drag preserving the grab offset.
    Thumb,
    /// On the track outside the thumb: starts a drag from the pressed row.
    Track,
}

/// Hit-tests a physical cursor position against the track and thumb.
///
/// Returns `None` outside the track (selection/mouse path keeps the event).
/// Positions over padding/gap bands never reach this function with an
/// inside-track answer because [`track_rect`] already excludes those bands.
#[must_use]
pub fn hit_test(
    track: TrackRect,
    thumb: ThumbSpan,
    cursor_x: f64,
    cursor_y: f64,
) -> Option<ScrollbarHit> {
    if !track.contains(cursor_x, cursor_y) {
        return None;
    }
    let rel_y = cursor_y - f64::from(track.y);
    let top = f64::from(thumb.y);
    if rel_y >= top && rel_y < top + f64::from(thumb.height) {
        Some(ScrollbarHit::Thumb)
    } else {
        Some(ScrollbarHit::Track)
    }
}

/// Maps a thumb-top position back to a scroll offset (drag inverse).
///
/// `thumb_y_px` is the desired thumb top in track space (the cursor y minus
/// the press-time grab offset the runtime retains), clamped to the travel
/// range; the result is `round((travel - y) / travel * scrollback_len)`
/// clamped to `[0, scrollback_len]`. Total over all inputs: non-finite or
/// out-of-range positions clamp, degenerate tracks yield live (`0`).
#[must_use]
pub fn offset_for_thumb_y(
    thumb_y_px: f64,
    track_height_px: u32,
    thumb_height_px: u32,
    scrollback_len: usize,
) -> usize {
    if scrollback_len == 0 || track_height_px == 0 {
        return 0;
    }
    if !thumb_y_px.is_finite() {
        return 0;
    }
    let travel = f64::from(track_height_px.saturating_sub(thumb_height_px));
    if travel <= 0.0 {
        return 0;
    }
    let y = thumb_y_px.clamp(0.0, travel);
    let frac = (travel - y) / travel;
    let offset = (frac * scrollback_len as f64).round();
    (offset as i64).clamp(0, scrollback_len as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK: u32 = 456; // 24 rows x 19px cells
    const ROWS: usize = 24;

    #[test]
    fn mode_parse_is_exact_and_fail_closed() {
        assert_eq!(ScrollbarMode::parse("hidden"), Some(ScrollbarMode::Hidden));
        assert_eq!(ScrollbarMode::parse("always"), Some(ScrollbarMode::Always));
        assert_eq!(ScrollbarMode::parse("auto"), Some(ScrollbarMode::Auto));
        assert_eq!(ScrollbarMode::parse("Hidden"), None);
        assert_eq!(ScrollbarMode::parse(" hidden"), None);
        assert_eq!(ScrollbarMode::parse(""), None);
        assert_eq!(ScrollbarMode::parse("overlay"), None);
        assert_eq!(ScrollbarMode::Hidden.as_str(), "hidden");
        assert_eq!(ScrollbarMode::Always.as_str(), "always");
        assert_eq!(ScrollbarMode::Auto.as_str(), "auto");
        assert_eq!(ScrollbarMode::default(), ScrollbarMode::Auto);
    }

    #[test]
    fn thumb_none_when_nothing_to_scroll_or_degenerate() {
        assert_eq!(thumb_geometry(TRACK, ROWS, 0, 0), None);
        assert_eq!(thumb_geometry(0, ROWS, 100, 0), None);
        assert_eq!(thumb_geometry(TRACK, 0, 100, 0), None);
    }

    #[test]
    fn thumb_rests_at_bottom_when_live() {
        // 24 rows, 24 lines of history: proportional half, live => bottom.
        let thumb = thumb_geometry(TRACK, ROWS, 24, 0).expect("thumb");
        assert_eq!(thumb.height, TRACK / 2);
        assert_eq!(thumb.y, TRACK - thumb.height);
    }

    #[test]
    fn thumb_rests_at_top_when_oldest() {
        let thumb = thumb_geometry(TRACK, ROWS, 24, 24).expect("thumb");
        assert_eq!(thumb.height, TRACK / 2);
        assert_eq!(thumb.y, 0);
    }

    #[test]
    fn thumb_middle_is_centered() {
        let thumb = thumb_geometry(TRACK, ROWS, 24, 12).expect("thumb");
        let travel = TRACK - thumb.height;
        assert_eq!(thumb.y, travel / 2);
    }

    #[test]
    fn thumb_clamps_offset_and_min_height() {
        // Offset past the end clamps to the top.
        let thumb = thumb_geometry(TRACK, ROWS, 24, 999).expect("thumb");
        assert_eq!(thumb.y, 0);
        // Huge scrollback: proportional height collapses to the minimum,
        // and the thumb still reaches both ends exactly.
        let sb = 100_000usize;
        let tiny = thumb_geometry(TRACK, ROWS, sb, 0).expect("thumb");
        assert_eq!(tiny.height, MIN_THUMB_HEIGHT_PX);
        assert_eq!(tiny.y, TRACK - MIN_THUMB_HEIGHT_PX);
        let top = thumb_geometry(TRACK, ROWS, sb, sb).expect("thumb");
        assert_eq!(top.y, 0);
        assert_eq!(top.height, MIN_THUMB_HEIGHT_PX);
    }

    #[test]
    fn thumb_never_exceeds_track() {
        // One history line: proportional height rounds near-full; must fit.
        let thumb = thumb_geometry(TRACK, ROWS, 1, 0).expect("thumb");
        assert!(thumb.y + thumb.height <= TRACK);
        let thumb = thumb_geometry(TRACK, ROWS, 1, 1).expect("thumb");
        assert!(thumb.y + thumb.height <= TRACK);
    }

    #[test]
    fn mode_gating() {
        assert!(!is_visible(ScrollbarMode::Hidden, true, true));
        assert!(!is_visible(ScrollbarMode::Hidden, true, false));
        assert!(is_visible(ScrollbarMode::Always, true, false));
        assert!(!is_visible(ScrollbarMode::Always, false, true));
        assert!(is_visible(ScrollbarMode::Auto, true, true));
        assert!(!is_visible(ScrollbarMode::Auto, true, false));
        assert!(!is_visible(ScrollbarMode::Auto, false, true));
    }

    #[test]
    fn track_rect_hugs_decorated_content_frame() {
        let spec = |x, y, w, h, pad, width| TrackSpec {
            content_x: x,
            content_y: y,
            content_w: w,
            content_h: h,
            pad_px: pad,
            width_px: width,
        };
        // Decorated content frame at (16, 27), 704x440, with 8px padding and
        // an 8px thumb: the track hugs the content right edge, offset by the
        // padding, spanning the full content height.
        let track = track_rect(spec(16, 27, 704, 440, 8, 8)).expect("track");
        assert_eq!(
            track,
            TrackRect {
                // x = 16 + 8 + 704 - 8 = 720, y = 27 + 8 = 35.
                x: 720,
                y: 35,
                width: 8,
                height: 440,
            }
        );
        // Zero-origin content, no padding: flush at the content right edge.
        let plain = track_rect(spec(0, 0, 704, 440, 0, 8)).expect("track");
        assert_eq!(
            plain,
            TrackRect {
                x: 696,
                y: 0,
                width: 8,
                height: 440,
            }
        );
        // Degenerate inputs paint nothing.
        assert_eq!(track_rect(spec(0, 0, 0, 440, 8, 8)), None);
        assert_eq!(track_rect(spec(0, 0, 704, 0, 8, 8)), None);
        assert_eq!(track_rect(spec(0, 0, 704, 440, 8, 0)), None);
        // Thumb wider than the content: nothing (never underflow the x math).
        assert_eq!(track_rect(spec(0, 0, 4, 440, 0, 8)), None);
    }

    #[test]
    fn hit_test_distinguishes_thumb_track_outside() {
        let track = TrackRect {
            x: 100,
            y: 20,
            width: 8,
            height: 400,
        };
        let thumb = ThumbSpan {
            y: 300,
            height: 100,
        };
        // On the thumb (bottom, live position).
        assert_eq!(
            hit_test(track, thumb, 104.0, 350.0),
            Some(ScrollbarHit::Thumb)
        );
        // Track above the thumb.
        assert_eq!(
            hit_test(track, thumb, 104.0, 100.0),
            Some(ScrollbarHit::Track)
        );
        // Left of the track: outside (selection keeps the event).
        assert_eq!(hit_test(track, thumb, 99.0, 350.0), None);
        // Below the track: outside.
        assert_eq!(hit_test(track, thumb, 104.0, 420.0), None);
        // Non-finite positions never hit.
        assert_eq!(hit_test(track, thumb, f64::NAN, 350.0), None);
    }

    #[test]
    fn proximity_zone_covers_track_plus_radius() {
        let track = TrackRect {
            x: 100,
            y: 20,
            width: 8,
            height: 400,
        };
        assert!(track.near(104.0, 200.0));
        // 16px left of the track edge (x=100): still engaged.
        assert!(track.near(84.0, 200.0));
        assert!(!track.near(83.0, 200.0));
        // Far away: not engaged.
        assert!(!track.near(0.0, 0.0));
        assert!(!track.near(f64::INFINITY, 200.0));
    }

    #[test]
    fn drag_mapping_round_trips_thumb_positions() {
        let sb = 24usize;
        for offset in [0, 6, 12, 18, 24] {
            let thumb = thumb_geometry(TRACK, ROWS, sb, offset).expect("thumb");
            let back = offset_for_thumb_y(f64::from(thumb.y), TRACK, thumb.height, sb);
            assert!(
                back.abs_diff(offset) <= 1,
                "offset {offset} -> y {} -> {back}",
                thumb.y
            );
        }
        // Ends are exact.
        let live = thumb_geometry(TRACK, ROWS, sb, 0).expect("thumb");
        assert_eq!(
            offset_for_thumb_y(f64::from(live.y), TRACK, live.height, sb),
            0
        );
        let top = thumb_geometry(TRACK, ROWS, sb, sb).expect("thumb");
        assert_eq!(
            offset_for_thumb_y(f64::from(top.y), TRACK, top.height, sb),
            sb
        );
        // Out-of-range and degenerate inputs clamp to live.
        assert_eq!(offset_for_thumb_y(-50.0, TRACK, live.height, sb), sb);
        assert_eq!(offset_for_thumb_y(9999.0, TRACK, live.height, sb), 0);
        assert_eq!(offset_for_thumb_y(10.0, TRACK, live.height, 0), 0);
        assert_eq!(offset_for_thumb_y(f64::NAN, TRACK, live.height, sb), 0);
    }
}
