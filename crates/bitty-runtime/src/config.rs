//! Owned configuration for the `bitty-runtime` orchestration.
//!
//! Configuration is declarative, total, and validated eagerly. No file I/O,
//! Lua, or style inference occurs here; the values are already resolved
//! Rust primitives. The grid dimensions remain logical (columns x rows);
//! pixel dimensions are derived from [`cell_width`]/[`cell_height`] when
//! mapping to [`bitty_platform::PhysicalSize`] and surface extents.
//!
//! The initial grid size defaults to [`bitty_term_state::GRID_COLUMNS`] x
//! [`bitty_term_state::GRID_ROWS`] (80x24) because terminal state has not
//! yet implemented resize reflow (the singular reflow algorithm deferred
//! under the terminal-state-rfc open items). Resizes before that lands only
//! reconfigure the GPU/software surface and the PTY window size, not the
//! grid memory itself — documented honestly in [`crate::Runtime`].

use crate::error::RuntimeError;
use crate::runtime::AnimationPolicy;

/// Maximum animation duration in milliseconds (RFC-0002 hard bound;
/// mirrors `bitty-config` `MAX_ANIMATION_DURATION_MS`).
pub const MAX_ANIMATION_DURATION_MS: u32 = 500;

/// Default lines scrolled per wheel notch (CTX-0185).
/// Mirrors `bitty-config` `DEFAULT_SCROLL_LINES_PER_NOTCH` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SCROLL_LINES_PER_NOTCH: u32 = 3;

/// Maximum lines per wheel notch (matches the per-frame scroll cap).
pub const MAX_SCROLL_LINES_PER_NOTCH: u32 = 32;

/// Default smooth-scroll pixels per wheel notch (CTX-0185).
/// Mirrors `bitty-config` `DEFAULT_SCROLL_PIXELS_PER_NOTCH` (see above).
pub const DEFAULT_SCROLL_PIXELS_PER_NOTCH: u32 = 16;

/// Maximum smooth-scroll pixels per wheel notch.
pub const MAX_SCROLL_PIXELS_PER_NOTCH: u32 = 256;

/// Default retained scrollback lines (CTX-0297 `terminal.scrollback`).
///
/// Mirrors `bitty_term_state::SCROLLBACK_DEFAULT_LINES` (kept as a re-export
/// so runtime retention and terminal-state retention cannot drift) and the
/// `bitty-config` `terminal.scrollback` default (`10 000`); the
/// `bitty-app` mapping pins the pairing.
pub const DEFAULT_SCROLLBACK_LINES: usize = bitty_term_state::SCROLLBACK_DEFAULT_LINES;

/// Hard maximum retained scrollback lines (CTX-0297).
///
/// Mirrors `bitty_term_state::SCROLLBACK_MAX_LINES` and the accepted
/// `bitty-config` bound for `terminal.scrollback` (`0..=100_000`).
/// [`RuntimeConfig::validate`] rejects values above it fail-closed so a
/// future bound drift cannot grow terminal memory without limit.
pub const MAX_SCROLLBACK_LINES: usize = bitty_term_state::SCROLLBACK_MAX_LINES;

/// Default selection auto-copy behavior (CTX-0191).
/// Mirrors `bitty-config` `DEFAULT_SELECTION_AUTO_COPY` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SELECTION_AUTO_COPY: bool = true;

/// Default focus-follows-mouse behavior (CTX-0260).
/// Mirrors `bitty-config` `DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE` (kept as a
/// local constant because `bitty-runtime` must not depend on
/// `bitty-config`; `bitty-app` maps the effective value across at startup
/// and the two defaults must stay equal — covered by a cross-crate test in
/// `bitty-app`). `false` preserves click-to-focus.
pub const DEFAULT_FOCUS_FOLLOWS_MOUSE: bool = false;

/// Default hover-activation delay (CTX-0334).
/// Mirrors `bitty-config` `DEFAULT_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS`;
/// `0` activates on pointer entry (CTX-0260 behavior).
pub const DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS: u32 = 0;

/// Maximum accepted hover-activation delay in milliseconds (CTX-0334).
/// Mirrors `bitty-config` `MAX_MOUSE_FOCUS_FOLLOWS_MOUSE_DELAY_MS`;
/// [`RuntimeConfig::validate`] rejects larger values fail-closed.
pub const MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS: u32 = 2_000;

/// Default inner panel gap in cells (CTX-0177).
/// Mirrors `bitty-config` `DEFAULT_LAYOUT_GAPS_IN` (kept local for the same
/// no-dependency reason; paired by value and pinned by a `bitty-app` test).
pub const DEFAULT_LAYOUT_GAPS_IN: u16 = 0;

/// Default outer panel gap in cells (CTX-0177).
/// Mirrors `bitty-config` `DEFAULT_LAYOUT_GAPS_OUT` (see above).
pub const DEFAULT_LAYOUT_GAPS_OUT: u16 = 0;

/// Maximum panel gap in cells (either axis, CTX-0177).
/// Mirrors `bitty-config` `MAX_LAYOUT_GAP_CELLS` (see above).
pub const MAX_LAYOUT_GAP_CELLS: u16 = 16;

/// Alias of `bitty_ui`'s Core-owned decoration bounds (CTX-0292; accepted
/// spec CTX-0118). `bitty-runtime` already depends on `bitty-ui`, so these
/// cannot drift from the solver's own constants.
pub const DEFAULT_DECORATION_GAPS_IN_PX: u16 = bitty_ui::DEFAULT_GAPS_IN_PX;

/// Alias of `bitty_ui::DEFAULT_GAPS_OUT_PX` (CTX-0292).
pub const DEFAULT_DECORATION_GAPS_OUT_PX: u16 = bitty_ui::DEFAULT_GAPS_OUT_PX;

/// Alias of `bitty_ui::DEFAULT_BORDER_PX` (CTX-0292).
pub const DEFAULT_DECORATION_BORDER_PX: u16 = bitty_ui::DEFAULT_BORDER_PX;

/// Alias of `bitty_ui::DEFAULT_RADIUS_PX` (CTX-0292).
pub const DEFAULT_DECORATION_RADIUS_PX: u16 = bitty_ui::DEFAULT_RADIUS_PX;

/// Alias of `bitty_ui::DEFAULT_CONTENT_INSET_PX` (CTX-0333).
pub const DEFAULT_DECORATION_CONTENT_INSET_PX: u16 = bitty_ui::DEFAULT_CONTENT_INSET_PX;

/// Alias of `bitty_ui::MAX_GAP_PX` (CTX-0292: `0..=32` logical px).
pub const MAX_DECORATION_GAP_PX: u16 = bitty_ui::MAX_GAP_PX;

/// Alias of `bitty_ui::MAX_BORDER_PX` (CTX-0292: `0..=8` logical px).
pub const MAX_DECORATION_BORDER_PX: u16 = bitty_ui::MAX_BORDER_PX;

/// Alias of `bitty_ui::MAX_RADIUS_PX` (CTX-0292: `0..=16` logical px).
pub const MAX_DECORATION_RADIUS_PX: u16 = bitty_ui::MAX_RADIUS_PX;

/// Alias of `bitty_ui::MAX_CONTENT_INSET_PX` (CTX-0333: `0..=32` logical px).
pub const MAX_DECORATION_CONTENT_INSET_PX: u16 = bitty_ui::MAX_CONTENT_INSET_PX;

/// Ratified default focused outline (CTX-0340 `#33CCFF`, opaque).
pub const DEFAULT_OUTLINE_FOCUSED: bitty_render::grid::Rgba8 = [0x33, 0xCC, 0xFF, 0xFF];

/// Ratified default idle outline (CTX-0340 `#595959AA`).
pub const DEFAULT_OUTLINE_IDLE: bitty_render::grid::Rgba8 = [0x59, 0x59, 0x59, 0xAA];

/// Maps a Core decoration validation failure to the runtime config error
/// (CTX-0292), naming the offending property without echoing user content.
pub(crate) fn decoration_runtime_error(err: bitty_ui::DecorationError) -> RuntimeError {
    let msg = match err {
        bitty_ui::DecorationError::GapsIn(_) => {
            "decoration.gaps_in must be within [0, 32] logical pixels"
        }
        bitty_ui::DecorationError::GapsOut(_) => {
            "decoration.gaps_out must be within [0, 32] logical pixels"
        }
        bitty_ui::DecorationError::Border(_) => {
            "decoration.border must be within [0, 8] logical pixels"
        }
        bitty_ui::DecorationError::Radius(_) => {
            "decoration.radius must be within [0, 16] logical pixels"
        }
        bitty_ui::DecorationError::ContentInset(_) => {
            "decoration.content_inset must be within [0, 32] logical pixels"
        }
    };
    RuntimeError::InvalidConfig(msg)
}

/// Default window padding in logical pixels (CTX-0223).
/// Mirrors `bitty-config` `WindowConfig` default (`padding: 8`; kept as a
/// local constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_WINDOW_PADDING: u32 = 8;

/// Maximum window padding in logical pixels (CTX-0223).
/// Mirrors `bitty-config` `WindowConfig::validate` (`must be <= 64`;
/// see above).
pub const MAX_WINDOW_PADDING: u32 = 64;

/// Default window corner radius in physical px (CTX-0241 S0).
/// Mirrors `bitty-config` `DEFAULT_WINDOW_RADIUS_PX` (`0`; kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_WINDOW_RADIUS_PX: u32 = 0;

/// Maximum window corner radius in physical px (CTX-0241 S0).
/// Mirrors `bitty-config` `MAX_WINDOW_RADIUS_PX` (`0..=24`; see above).
pub const MAX_WINDOW_RADIUS_PX: u32 = 24;

/// Default scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `DEFAULT_SCROLLBAR_WIDTH` (kept as a local
/// constant because `bitty-runtime` must not depend on `bitty-config`;
/// `bitty-app` maps the effective value across at startup and the two
/// defaults must stay equal — covered by a cross-crate test in `bitty-app`).
pub const DEFAULT_SCROLLBAR_WIDTH: u32 = 8;

/// Minimum scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `MIN_SCROLLBAR_WIDTH_PX` (see above).
pub const MIN_SCROLLBAR_WIDTH_PX: u32 = 1;

/// Maximum scrollbar thumb width in logical pixels (CTX-0181).
/// Mirrors `bitty-config` `MAX_SCROLLBAR_WIDTH_PX` (see above).
pub const MAX_SCROLLBAR_WIDTH_PX: u32 = 32;

/// Per-window font zoom step in points (CTX-0263).
///
/// One mainstream step per chord press (Alacritty/Ghostty-class `1pt`);
/// the `0.5pt` finer alternative stays available via a direct
/// `set_font_size` call, but the default chord moves a full point so each
/// press is visibly distinct on HiDPI and headless captures.
pub const FONT_ZOOM_STEP_PT: f32 = 1.0;

/// Minimum live font size reachable by zoom (CTX-0263, fail-closed).
pub const FONT_ZOOM_MIN_PT: f32 = 6.0;

/// Maximum live font size reachable by zoom (CTX-0263, fail-closed).
pub const FONT_ZOOM_MAX_PT: f32 = 32.0;

/// Owned runtime configuration, validated on construction.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeConfig {
    /// Terminal width in columns.
    pub cols: usize,
    /// Terminal height in rows.
    pub rows: usize,
    /// Cell width in physical pixels.
    ///
    /// Default `9` (CTX-0157): legacy `8` plus `1px` letter-spacing breathing
    /// room. Measured `JetBrainsMono Nerd Font` at 12pt advances `9.6px`;
    /// ghostty uses pure font metrics, kitty 11pt advances ~`8.8px` — `9`
    /// is the conservative readable default (see `bitty-config` `FontConfig`
    /// docs for the full ghostty/kitty table).
    pub cell_width: u32,
    /// Cell height in physical pixels.
    ///
    /// Default `19` (CTX-0157): `round(16 * 1.2)` line-height breathing room.
    /// True JetBrains Mono line at 12pt is `21.1px`; `19` sits between legacy
    /// `16` and true `21`, matching kitty 11pt line (~`19.4px`).
    pub cell_height: u32,
    /// Capacity of the bounded cold-path event queue.
    pub cold_queue_capacity: usize,
    /// Font family used by the renderer; whitespace-trimmed on validation.
    ///
    /// Default [`crate::font_default_family`] (`JetBrainsMono Nerd Font`):
    /// Nerd-patched for starship/opencode glyphs, matching the CTX-0157
    /// ghostty side-by-side acceptance at 12pt. System `monospace` remains
    /// the ultimate fallback (see `bitty-config` `FONT_FALLBACK_CHAIN`).
    pub font_family: String,
    /// Font point size; must be finite and within `(0, 3999]`.
    pub font_size: f32,
    /// Lines scrolled per wheel notch, `1..=32` (CTX-0185; default 3).
    /// Applied to `Lines` deltas directly and to `Pixels` deltas via the
    /// notch equivalence (`scroll_pixels_per_notch` px = one notch).
    /// Direction semantics are unchanged (positive = up into history).
    pub scroll_lines_per_notch: u32,
    /// Smooth-scroll pixels per wheel notch, `1..=256` (CTX-0185; default 16).
    pub scroll_pixels_per_notch: u32,
    /// Retained scrollback lines captured when terminal state is created
    /// (CTX-0297 `terminal.scrollback`). `0..=MAX_SCROLLBACK_LINES`; default
    /// `DEFAULT_SCROLLBACK_LINES` (`10 000`). `0` disables scrollback
    /// retention. Consumed by `State::with_scrollback_lines`; the reload
    /// class is `RestartRequired` because already-created terminals keep
    /// their captured capacity.
    pub scrollback: usize,
    /// Whether a committed mouse selection auto-copies to the clipboard
    /// (CTX-0191; default `true` = ghostty-class copy-on-select).
    /// `false` leaves the highlight in place; the explicit
    /// `copy_to_clipboard` chord (Ctrl+Shift+C) still copies.
    pub selection_auto_copy: bool,
    /// Whether hover moves keyboard focus to the hovered pane (CTX-0260
    /// `mouse.focus_follows_mouse`; default `false` = click-to-focus).
    /// When `false`, hover never touches focus; when `true`, cursor motion
    /// over another pane moves keyboard focus there (Shift still forces the
    /// selection path and suppresses hover-focus).
    pub focus_follows_mouse: bool,
    /// Dwell time before hover activation moves focus (CTX-0334
    /// `mouse.focus_follows_mouse_delay_ms`). `0` (default) activates on
    /// pointer entry; a positive value requires the pointer to remain in the
    /// hovered pane for at least this long, so a transient pass-through
    /// never steals focus. Bounded by [`MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS`].
    pub focus_follows_mouse_delay: std::time::Duration,
    /// Spacing between sibling panes in cells (CTX-0177 `layout.gaps_in`).
    /// `0..=MAX_LAYOUT_GAP_CELLS`; default `0` = edge-to-edge tiling.
    /// The gap band shows the window background; per-leaf rendering and
    /// hit-testing exclude it.
    pub gaps_in: u16,
    /// Inset around the container edge in cells (CTX-0177
    /// `layout.gaps_out`). Same bounds and default as `gaps_in`.
    pub gaps_out: u16,
    /// Core-owned workspace decoration in logical pixels (CTX-0292; accepted
    /// spec CTX-0118 defaults `4/6/2/6`). Decoration is never part of a
    /// `LayoutTree` or a plugin proposal; it is carried here from the
    /// validated `EffectiveConfig` and applied by
    /// [`crate::Runtime::decorated_allocations`] / the future live present
    /// stage.
    pub decoration: bitty_ui::Decoration,
    /// Focused/idle outline colors (CTX-0340). Resolved by `bitty-config`
    /// from the theme token / `decoration.border_color` / explicit pair and
    /// carried here for the per-`View` paint decision. Defaults to the
    /// ratified `#33CCFF` / `#595959AA` pair.
    pub outline_focused: bitty_render::grid::Rgba8,
    /// Idle outline color; see [`Self::outline_focused`].
    pub outline_idle: bitty_render::grid::Rgba8,
    /// Window padding in logical pixels on every side (CTX-0223
    /// `window.padding`). `0..=MAX_WINDOW_PADDING`; default
    /// `DEFAULT_WINDOW_PADDING` (`8`, ghostty/alacritty-class breathing
    /// room). The padding band shows the window background; the grid is
    /// translated by the inset origin and grid derivation subtracts twice
    /// the padding before dividing by the cell metrics, so the window —
    /// not the grid — absorbs the inset.
    pub window_padding: u32,
    /// Window corner radius in physical px (CTX-0241 S0 `window.radius_px`).
    /// `0..=MAX_WINDOW_RADIUS_PX`; default `DEFAULT_WINDOW_RADIUS_PX` (`0`
    /// = square corners). S0 is a parsed no-op: accepted, stored, and
    /// reported, with zero render effect (no DrawList/present consumer reads
    /// this field). Default 0 keeps every path on the zero-cost fast path.
    pub window_radius_px: u32,
    /// Overlay scrollbar display mode (CTX-0181 `scrollbar.mode`).
    /// Default `Hidden` = geometry-neutral (zero pixels, zero layout delta).
    /// `Always` paints the thumb whenever scrollback exists; `Auto` reveals
    /// it on mouse proximity/hover/drag.
    pub scrollbar_mode: bitty_ui::ScrollbarMode,
    /// Overlay scrollbar thumb width in logical pixels (CTX-0181
    /// `scrollbar.width`). `MIN_SCROLLBAR_WIDTH_PX..=MAX_SCROLLBAR_WIDTH_PX`;
    /// default `DEFAULT_SCROLLBAR_WIDTH` (`8`). Scaled by the live DPI
    /// factor exactly like `window_padding`; the track lives inside the
    /// leaf allocation so the grid never absorbs it.
    pub scrollbar_width: u32,
    /// Resolved renderer-side panel animation policy (RFC-0002, CTX-0341).
    ///
    /// Carries the accepted durations/easings, the `reduced_motion` mode, and
    /// the `safe_mode` latch. The in-flight [`crate::runtime::PanelAnimator`]
    /// tracker lives on [`crate::Runtime`] because it holds wall-clock state;
    /// this is the immutable contract it is armed from.
    pub animations: AnimationPolicy,
}

/// Default cell width in logical pixels (CTX-0157 breathing-room cell).
pub const DEFAULT_CELL_WIDTH: u32 = 9;

/// Default cell height in logical pixels (CTX-0157 breathing-room cell).
pub const DEFAULT_CELL_HEIGHT: u32 = 19;

/// Default font family (CTX-0157 acceptance probe).
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";

/// Returns the default font family.
#[must_use]
pub fn font_default_family() -> String {
    DEFAULT_FONT_FAMILY.to_string()
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            cols: bitty_term_state::GRID_COLUMNS,
            rows: bitty_term_state::GRID_ROWS,
            cell_width: DEFAULT_CELL_WIDTH,
            cell_height: DEFAULT_CELL_HEIGHT,
            cold_queue_capacity: 256,
            font_family: font_default_family(),
            font_size: 12.0,
            scroll_lines_per_notch: DEFAULT_SCROLL_LINES_PER_NOTCH,
            scroll_pixels_per_notch: DEFAULT_SCROLL_PIXELS_PER_NOTCH,
            scrollback: DEFAULT_SCROLLBACK_LINES,
            selection_auto_copy: DEFAULT_SELECTION_AUTO_COPY,
            focus_follows_mouse: DEFAULT_FOCUS_FOLLOWS_MOUSE,
            focus_follows_mouse_delay: std::time::Duration::from_millis(u64::from(
                DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
            )),
            gaps_in: DEFAULT_LAYOUT_GAPS_IN,
            gaps_out: DEFAULT_LAYOUT_GAPS_OUT,
            decoration: bitty_ui::Decoration::default(),
            outline_focused: DEFAULT_OUTLINE_FOCUSED,
            outline_idle: DEFAULT_OUTLINE_IDLE,
            window_padding: DEFAULT_WINDOW_PADDING,
            window_radius_px: DEFAULT_WINDOW_RADIUS_PX,
            scrollbar_mode: bitty_ui::ScrollbarMode::Hidden,
            scrollbar_width: DEFAULT_SCROLLBAR_WIDTH,
            animations: AnimationPolicy::default(),
        }
    }
}

impl RuntimeConfig {
    /// Validates and builds a config. All fields are checked for
    /// total, deterministic construction.
    ///
    /// CTX-0260: `focus_follows_mouse` is not a `new()` parameter (adding
    /// one would churn every call site); it defaults off here and the app
    /// layer sets it post-construction from the effective config.
    ///
    /// CTX-0292: `decoration` follows the same pattern: it defaults to the
    /// accepted CTX-0118 values here and the app layer assigns the validated
    /// effective decoration post-construction; [`Self::validate`] rejects
    /// out-of-range decoration values fail-closed.
    ///
    /// CTX-0297: `scrollback` follows the same pattern: it defaults to
    /// [`DEFAULT_SCROLLBACK_LINES`] here and the app layer assigns the
    /// effective `terminal.scrollback` post-construction;
    /// [`Self::validate`] rejects values above
    /// [`MAX_SCROLLBACK_LINES`] fail-closed.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when any field is outside its
    /// documented range.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cols: usize,
        rows: usize,
        cell_width: u32,
        cell_height: u32,
        cold_queue_capacity: usize,
        font_family: impl Into<String>,
        font_size: f32,
        scroll_lines_per_notch: u32,
        scroll_pixels_per_notch: u32,
        selection_auto_copy: bool,
        gaps_in: u16,
        gaps_out: u16,
        window_padding: u32,
        window_radius_px: u32,
        scrollbar_mode: bitty_ui::ScrollbarMode,
        scrollbar_width: u32,
    ) -> Result<Self, RuntimeError> {
        let font_family = font_family.into();
        let cfg = Self {
            cols,
            rows,
            cell_width,
            cell_height,
            cold_queue_capacity,
            font_family,
            font_size,
            scroll_lines_per_notch,
            scroll_pixels_per_notch,
            scrollback: DEFAULT_SCROLLBACK_LINES,
            selection_auto_copy,
            focus_follows_mouse: DEFAULT_FOCUS_FOLLOWS_MOUSE,
            focus_follows_mouse_delay: std::time::Duration::from_millis(u64::from(
                DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS,
            )),
            gaps_in,
            gaps_out,
            decoration: bitty_ui::Decoration::default(),
            outline_focused: DEFAULT_OUTLINE_FOCUSED,
            outline_idle: DEFAULT_OUTLINE_IDLE,
            window_padding,
            window_radius_px,
            scrollbar_mode,
            scrollbar_width,
            animations: AnimationPolicy::default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validates an already constructed config.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when any field violates its contract.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.cols == 0 || self.rows == 0 {
            return Err(RuntimeError::InvalidConfig("cols and rows must be >= 1"));
        }
        if self.cell_width == 0 || self.cell_height == 0 {
            return Err(RuntimeError::InvalidConfig(
                "cell_width and cell_height must be >= 1",
            ));
        }
        if self.cold_queue_capacity == 0 {
            return Err(RuntimeError::InvalidQueueCapacity);
        }
        if self.font_family.trim().is_empty() {
            return Err(RuntimeError::InvalidConfig("font_family must not be empty"));
        }
        if !(self.font_size.is_finite() && self.font_size > 0.0 && self.font_size <= 3999.0) {
            return Err(RuntimeError::InvalidConfig(
                "font_size must be finite within (0, 3999]",
            ));
        }
        if !(1..=MAX_SCROLL_LINES_PER_NOTCH).contains(&self.scroll_lines_per_notch) {
            return Err(RuntimeError::InvalidConfig(
                "scroll_lines_per_notch must be within [1, 32]",
            ));
        }
        if !(1..=MAX_SCROLL_PIXELS_PER_NOTCH).contains(&self.scroll_pixels_per_notch) {
            return Err(RuntimeError::InvalidConfig(
                "scroll_pixels_per_notch must be within [1, 256]",
            ));
        }
        if self.scrollback > MAX_SCROLLBACK_LINES {
            return Err(RuntimeError::InvalidConfig(
                "scrollback must be within [0, 100000] lines",
            ));
        }
        if self.cols > bitty_term_state::MAX_GRID_DIM || self.rows > bitty_term_state::MAX_GRID_DIM
        {
            return Err(RuntimeError::InvalidConfig(
                "grid dimensions must be <= 1000",
            ));
        }
        if self.gaps_in > MAX_LAYOUT_GAP_CELLS || self.gaps_out > MAX_LAYOUT_GAP_CELLS {
            return Err(RuntimeError::InvalidConfig(
                "layout gaps must be within [0, 16] cells",
            ));
        }
        if self.focus_follows_mouse_delay
            > std::time::Duration::from_millis(u64::from(MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS))
        {
            return Err(RuntimeError::InvalidConfig(
                "focus_follows_mouse_delay must be within [0, 2000] milliseconds",
            ));
        }
        if let Err(err) = self.decoration.validate() {
            return Err(decoration_runtime_error(err));
        }
        if self.window_padding > MAX_WINDOW_PADDING {
            return Err(RuntimeError::InvalidConfig(
                "window_padding must be within [0, 64] logical pixels",
            ));
        }
        if self.window_radius_px > MAX_WINDOW_RADIUS_PX {
            return Err(RuntimeError::InvalidConfig(
                "window_radius_px must be within [0, 24] physical pixels",
            ));
        }
        if !(MIN_SCROLLBAR_WIDTH_PX..=MAX_SCROLLBAR_WIDTH_PX).contains(&self.scrollbar_width) {
            return Err(RuntimeError::InvalidConfig(
                "scrollbar_width must be within [1, 32] logical pixels",
            ));
        }
        // RFC-0002 (CTX-0341): every resolved animation duration is
        // fail-closed within the accepted `0..=500` ms hard bound. The
        // config layer already enforces this; the runtime repeats it so a
        // direct `RuntimeConfig` construction can never arm an unbounded
        // animation.
        for ms in self.animations.duration_ms {
            if ms > MAX_ANIMATION_DURATION_MS {
                return Err(RuntimeError::InvalidConfig(
                    "animation durations must be within [0, 500] milliseconds",
                ));
            }
        }
        Ok(())
    }

    /// Pixel extent for the current grid geometry, excluding window padding.
    ///
    /// Unchanged by CTX-0223: this stays the grid-space extent so existing
    /// geometry (grid derivation round-trips, resize math) keeps its
    /// meaning. Use [`Self::window_extent`] for the surface/window size.
    #[must_use]
    pub fn pixel_extent(&self) -> bitty_platform::PhysicalSize {
        let w = u64::from(self.cell_width) * self.cols as u64;
        let h = u64::from(self.cell_height) * self.rows as u64;
        let w = if w > u32::MAX as u64 {
            u32::MAX
        } else {
            w as u32
        };
        let h = if h > u32::MAX as u64 {
            u32::MAX
        } else {
            h as u32
        };
        bitty_platform::PhysicalSize::new(w, h)
    }

    /// Derives `cols`/`rows` from a physical size using the configured cell
    /// metrics, saturating to at least 1x1.
    #[must_use]
    pub fn grid_from_pixels(&self, size: bitty_platform::PhysicalSize) -> (usize, usize) {
        let cols = (size.width() / self.cell_width).max(1) as usize;
        let rows = (size.height() / self.cell_height).max(1) as usize;
        (
            cols.min(bitty_term_state::MAX_GRID_DIM),
            rows.min(bitty_term_state::MAX_GRID_DIM),
        )
    }

    /// Window (surface) extent: grid pixels plus the padding inset on every
    /// side (CTX-0223). Saturates instead of overflowing on hostile inputs.
    /// At scale 1.0 logical pixels equal physical pixels, so this is the
    /// extent [`crate::Runtime`] configures its surface with; HiDPI callers
    /// scale the padding via the live scale factor first (see
    /// [`crate::Runtime::window_padding_physical`]).
    #[must_use]
    pub fn window_extent(&self) -> bitty_platform::PhysicalSize {
        let grid = self.pixel_extent();
        let inset = u64::from(self.window_padding.min(MAX_WINDOW_PADDING)).saturating_mul(2);
        let w = u64::from(grid.width()).saturating_add(inset);
        let h = u64::from(grid.height()).saturating_add(inset);
        bitty_platform::PhysicalSize::new(
            w.min(u32::MAX as u64) as u32,
            h.min(u32::MAX as u64) as u32,
        )
    }

    /// Derives `cols`/`rows` from a window (surface) size by first removing
    /// the padding inset on every side, then dividing by the cell metrics
    /// (CTX-0223). Saturates to at least 1x1; a padding that covers the
    /// window still addresses one cell (the caller keeps the previous
    /// geometry when the inset leaves no drawable content).
    #[must_use]
    pub fn grid_from_window_pixels(&self, size: bitty_platform::PhysicalSize) -> (usize, usize) {
        let inset = self
            .window_padding
            .min(MAX_WINDOW_PADDING)
            .saturating_mul(2);
        let content = bitty_platform::PhysicalSize::new(
            size.width().saturating_sub(inset),
            size.height().saturating_sub(inset),
        );
        self.grid_from_pixels(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        RuntimeConfig::default()
            .validate()
            .expect("default must be valid");
    }

    #[test]
    fn animation_durations_are_bounded_fail_closed() {
        // RFC-0002 (CTX-0341): every resolved duration is `0..=500`; the
        // runtime repeats the bound so a direct construction cannot arm an
        // unbounded animation.
        let mut cfg = RuntimeConfig::default();
        cfg.animations.duration_ms = [0, 0, 0, 0];
        cfg.validate().expect("0 ms boundary valid");
        cfg.animations.duration_ms = [500, 500, 500, 500];
        cfg.validate().expect("500 ms boundary valid");
        for idx in 0..4 {
            let mut bad = RuntimeConfig::default();
            bad.animations.duration_ms[idx] = MAX_ANIMATION_DURATION_MS + 1;
            bad.validate()
                .expect_err("out-of-range animation duration must fail closed");
        }
    }

    #[test]
    fn pixel_extent_derivation() {
        let cfg = RuntimeConfig::default();
        let extent = cfg.pixel_extent();
        // 80x24 at breathing-room 9x19 (CTX-0157).
        assert_eq!(extent.width(), 720);
        assert_eq!(extent.height(), 456);
    }

    #[test]
    fn grid_from_pixels_rounds_down_and_clamps() {
        let cfg = RuntimeConfig::default();
        let size = bitty_platform::PhysicalSize::new(100, 100);
        let (cols, rows) = cfg.grid_from_pixels(size);
        assert_eq!(cols, 11);
        assert_eq!(rows, 5);
        let zero = bitty_platform::PhysicalSize::new(0, 0);
        let (c, r) = cfg.grid_from_pixels(zero);
        assert_eq!(c, 1);
        assert_eq!(r, 1);
        let huge = bitty_platform::PhysicalSize::new(u32::MAX, u32::MAX);
        let (c, r) = cfg.grid_from_pixels(huge);
        assert_eq!(
            (c, r),
            (
                bitty_term_state::MAX_GRID_DIM,
                bitty_term_state::MAX_GRID_DIM
            )
        );
    }

    #[test]
    fn grid_dimension_bound_is_term_state_max_grid_dim() {
        // One shared bound: runtime validation and the term-state resize
        // clamp must both use `MAX_GRID_DIM` or state would silently clamp
        // below an accepted config.
        assert_eq!(bitty_term_state::MAX_GRID_DIM, 1000);
        RuntimeConfig {
            cols: bitty_term_state::MAX_GRID_DIM,
            rows: bitty_term_state::MAX_GRID_DIM,
            ..RuntimeConfig::default()
        }
        .validate()
        .expect("bound itself is valid");
        let over = bitty_term_state::MAX_GRID_DIM + 1;
        assert!(
            RuntimeConfig {
                cols: over,
                ..RuntimeConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            RuntimeConfig {
                rows: over,
                ..RuntimeConfig::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn invalid_fields_are_rejected() {
        assert!(
            RuntimeConfig::new(
                0,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                0,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                0,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "   ",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                0.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn scroll_speed_fields_are_rejected_out_of_range() {
        // CTX-0185: scroll speed is validated fail-closed like other config.
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                0,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                33,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                3,
                0,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                8,
                16,
                256,
                "mono",
                12.0,
                3,
                257,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        RuntimeConfig::new(
            80,
            24,
            8,
            16,
            256,
            "mono",
            12.0,
            1,
            1,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("scroll speed boundaries must be valid");
        RuntimeConfig::new(
            80,
            24,
            8,
            16,
            256,
            "mono",
            12.0,
            32,
            256,
            false,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("scroll speed boundaries must be valid");
    }

    #[test]
    fn focus_follows_mouse_defaults_off_and_accepts_both() {
        // CTX-0260: default-off preserves click-to-focus; both values are
        // total (booleans always validate). `new()` defaults off; callers
        // opt in post-construction.
        const { assert!(!DEFAULT_FOCUS_FOLLOWS_MOUSE) }
        const { assert!(DEFAULT_FOCUS_FOLLOWS_MOUSE_DELAY_MS == 0) }
        assert!(!RuntimeConfig::default().focus_follows_mouse);
        assert_eq!(
            RuntimeConfig::default().focus_follows_mouse_delay,
            std::time::Duration::ZERO
        );
        let cfg = RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("new defaults hover-focus off");
        assert!(!cfg.focus_follows_mouse);
        let opt_in = RuntimeConfig {
            focus_follows_mouse: true,
            ..RuntimeConfig::default()
        };
        opt_in.validate().expect("opt-in valid");
        let opt_out = RuntimeConfig {
            focus_follows_mouse: false,
            ..RuntimeConfig::default()
        };
        opt_out.validate().expect("opt-out valid");
        // CTX-0334: the dwell delay is bounded fail-closed.
        for good in [
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(u64::from(MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS)),
        ] {
            RuntimeConfig {
                focus_follows_mouse_delay: good,
                ..RuntimeConfig::default()
            }
            .validate()
            .expect("boundary delay must be valid");
        }
        RuntimeConfig {
            focus_follows_mouse_delay: std::time::Duration::from_millis(
                u64::from(MAX_FOCUS_FOLLOWS_MOUSE_DELAY_MS) + 1,
            ),
            ..RuntimeConfig::default()
        }
        .validate()
        .expect_err("over-max delay must fail closed");
    }

    #[test]
    fn scrollback_default_and_bounds() {
        // CTX-0297: retention defaults to the shared 10 000 and the hard
        // 100 000 bound fails closed; `0` (retention disabled) stays valid.
        const { assert!(DEFAULT_SCROLLBACK_LINES == 10_000) }
        const { assert!(MAX_SCROLLBACK_LINES == 100_000) }
        assert_eq!(
            RuntimeConfig::default().scrollback,
            DEFAULT_SCROLLBACK_LINES
        );
        RuntimeConfig {
            scrollback: MAX_SCROLLBACK_LINES,
            ..RuntimeConfig::default()
        }
        .validate()
        .expect("hard max builds");
        RuntimeConfig {
            scrollback: 0,
            ..RuntimeConfig::default()
        }
        .validate()
        .expect("retention disabled builds");
        assert!(
            RuntimeConfig {
                scrollback: MAX_SCROLLBACK_LINES + 1,
                ..RuntimeConfig::default()
            }
            .validate()
            .is_err(),
            "above hard max must fail closed"
        );
    }

    #[test]
    fn selection_auto_copy_defaults_on_and_accepts_both() {
        // CTX-0191: default-on preserves copy-on-select; both values build.
        const { assert!(DEFAULT_SELECTION_AUTO_COPY) }
        assert!(RuntimeConfig::default().selection_auto_copy);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("auto-copy on builds");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            false,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("auto-copy off builds");
    }

    #[test]
    fn scrollbar_defaults_hidden_and_validates_bounds() {
        // CTX-0181: hidden-by-default keeps geometry neutral; width bounds
        // fail closed (mirrors `bitty-config` bounds, pinned in `bitty-app`).
        const { assert!(DEFAULT_SCROLLBAR_WIDTH == 8) }
        const { assert!(MIN_SCROLLBAR_WIDTH_PX == 1) }
        const { assert!(MAX_SCROLLBAR_WIDTH_PX == 32) }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.scrollbar_mode, bitty_ui::ScrollbarMode::Hidden);
        assert_eq!(cfg.scrollbar_width, DEFAULT_SCROLLBAR_WIDTH);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Auto,
            12,
        )
        .expect("auto scrollbar builds");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                0,
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                33,
            )
            .is_err()
        );
    }

    #[test]
    fn default_matches_readable_cell_and_nerd_font() {
        let cfg = RuntimeConfig::default();
        assert_eq!(
            (cfg.cell_width, cfg.cell_height),
            (DEFAULT_CELL_WIDTH, DEFAULT_CELL_HEIGHT)
        );
        assert_eq!((DEFAULT_CELL_WIDTH, DEFAULT_CELL_HEIGHT), (9, 19));
        assert_eq!(cfg.font_family, "JetBrainsMono Nerd Font");
        assert!((cfg.font_size - 12.0).abs() < f32::EPSILON);
    }

    #[test]
    fn layout_gaps_default_zero_and_validate_bounds() {
        // CTX-0177: defaults preserve edge-to-edge tiling; bounds fail closed.
        const { assert!(DEFAULT_LAYOUT_GAPS_IN == 0) }
        const { assert!(DEFAULT_LAYOUT_GAPS_OUT == 0) }
        const { assert!(MAX_LAYOUT_GAP_CELLS == 16) }
        let cfg = RuntimeConfig::default();
        assert_eq!((cfg.gaps_in, cfg.gaps_out), (0, 0));
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero gaps build");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            16,
            16,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("max gaps build");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                17,
                0,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                17,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                u16::MAX,
                u16::MAX,
                8,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn decoration_defaults_match_unified_spec_and_validate() {
        // CTX-0292/CTX-0333: unified defaults 6/6/2/6/6 logical px; runtime
        // constants alias the bitty-ui solver bounds so they cannot drift;
        // out-of-range values fail closed naming the property.
        const { assert!(DEFAULT_DECORATION_GAPS_IN_PX == 6) }
        const { assert!(DEFAULT_DECORATION_GAPS_OUT_PX == 6) }
        const { assert!(DEFAULT_DECORATION_BORDER_PX == 2) }
        const { assert!(DEFAULT_DECORATION_RADIUS_PX == 6) }
        const { assert!(DEFAULT_DECORATION_CONTENT_INSET_PX == 6) }
        const { assert!(MAX_DECORATION_GAP_PX == 32) }
        const { assert!(MAX_DECORATION_BORDER_PX == 8) }
        const { assert!(MAX_DECORATION_RADIUS_PX == 16) }
        const { assert!(MAX_DECORATION_CONTENT_INSET_PX == 32) }
        let cfg = RuntimeConfig::default();
        assert_eq!((cfg.decoration.gaps_in, cfg.decoration.gaps_out), (6, 6));
        assert_eq!((cfg.decoration.border, cfg.decoration.radius), (2, 6));
        assert_eq!(cfg.decoration.content_inset, 6);
        // CTX-0333: sibling and container gaps match by default.
        assert_eq!(cfg.decoration.gaps_in, cfg.decoration.gaps_out);
        cfg.validate().expect("default decoration valid");
        // `new()` leaves decoration at the accepted defaults.
        let built = RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("build");
        assert_eq!(built.decoration, bitty_ui::Decoration::default());
        // Out-of-range decoration fails closed at validate().
        for (field, bad) in [
            ("gaps_in", bitty_ui::Decoration::new(33, 6, 2, 6, 6)),
            ("gaps_out", bitty_ui::Decoration::new(6, 33, 2, 6, 6)),
            ("border", bitty_ui::Decoration::new(6, 6, 9, 6, 6)),
            ("radius", bitty_ui::Decoration::new(6, 6, 2, 17, 6)),
            ("content_inset", bitty_ui::Decoration::new(6, 6, 2, 6, 33)),
        ] {
            let cfg = RuntimeConfig {
                decoration: bad,
                ..RuntimeConfig::default()
            };
            let err = cfg.validate().expect_err("out-of-range must fail");
            let msg = err.to_string();
            assert!(msg.contains(field), "{field}: {msg}");
        }
    }

    #[test]
    fn window_padding_default_and_bounds() {
        // CTX-0223: default 8px mirrors `bitty-config` `WindowConfig`
        // (pinned by value in `bitty-app`); bounds fail closed.
        const { assert!(DEFAULT_WINDOW_PADDING == 8) }
        const { assert!(MAX_WINDOW_PADDING == 64) }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.window_padding, DEFAULT_WINDOW_PADDING);
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            0,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero padding builds");
        RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            64,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("max padding builds");
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                65,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
        assert!(
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                u32::MAX,
                DEFAULT_WINDOW_RADIUS_PX,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH
            )
            .is_err()
        );
    }

    #[test]
    fn window_extent_adds_padding_around_grid() {
        // 80x24 at 9x19 = 720x456 grid; default 8px padding => 736x472 window.
        let cfg = RuntimeConfig::default();
        assert_eq!(
            cfg.pixel_extent(),
            bitty_platform::PhysicalSize::new(720, 456)
        );
        assert_eq!(
            cfg.window_extent(),
            bitty_platform::PhysicalSize::new(736, 472)
        );
        let bare = RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            0,
            DEFAULT_WINDOW_RADIUS_PX,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("zero padding builds");
        assert_eq!(bare.window_extent(), bare.pixel_extent());
    }

    #[test]
    fn grid_from_window_pixels_removes_padding_first() {
        let cfg = RuntimeConfig::default();
        // 736x472 window minus 2x8px padding = 720x456 grid = 80x24 cells.
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(736, 472)),
            (80, 24)
        );
        // Resize math: 800x600 window => (800-16)/9 x (600-16)/19 = 87x30.
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(800, 600)),
            (87, 30)
        );
        // Padding-covered windows still address one cell (fail-soft floor).
        assert_eq!(
            cfg.grid_from_window_pixels(bitty_platform::PhysicalSize::new(10, 10)),
            (1, 1)
        );
    }

    #[test]
    fn window_radius_default_and_bounds() {
        // CTX-0241 S0: default 0 = square no-op (zero-cost fast path);
        // bounds `0..=24` fail closed. Radius never affects extents
        // (no-op proof lives in `tests/window_radius_noop.rs`).
        const { assert!(DEFAULT_WINDOW_RADIUS_PX == 0) }
        const { assert!(MAX_WINDOW_RADIUS_PX == 24) }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.window_radius_px, DEFAULT_WINDOW_RADIUS_PX);
        for radius in [0, 1, 12, MAX_WINDOW_RADIUS_PX] {
            RuntimeConfig::new(
                80,
                24,
                9,
                19,
                256,
                "mono",
                12.0,
                3,
                16,
                true,
                0,
                0,
                8,
                radius,
                bitty_ui::ScrollbarMode::Hidden,
                DEFAULT_SCROLLBAR_WIDTH,
            )
            .expect("radius in range builds");
        }
        for bad in [MAX_WINDOW_RADIUS_PX + 1, u32::MAX] {
            assert!(
                RuntimeConfig::new(
                    80,
                    24,
                    9,
                    19,
                    256,
                    "mono",
                    12.0,
                    3,
                    16,
                    true,
                    0,
                    0,
                    8,
                    bad,
                    bitty_ui::ScrollbarMode::Hidden,
                    DEFAULT_SCROLLBAR_WIDTH,
                )
                .is_err(),
                "radius {bad} must fail closed"
            );
        }
        // Radius never changes the grid/window extents (parsed no-op).
        let with_radius = RuntimeConfig::new(
            80,
            24,
            9,
            19,
            256,
            "mono",
            12.0,
            3,
            16,
            true,
            0,
            0,
            8,
            12,
            bitty_ui::ScrollbarMode::Hidden,
            DEFAULT_SCROLLBAR_WIDTH,
        )
        .expect("radius builds");
        assert_eq!(with_radius.pixel_extent(), cfg.pixel_extent());
        assert_eq!(with_radius.window_extent(), cfg.window_extent());
    }
}
