//! `Runtime` — live-adopt setters for config reload (CTX-0898, issue #1522).
//!
//! The CTX-0814 reload path adopts the presentation subset through the
//! existing setters (`set_decoration`, `set_outline`, `set_font_size`, ...).
//! This module adds the remaining runtime-owned `Live` fields:
//!
//! * [`Runtime::set_theme_palette`] — `appearance.theme` / `appearance.colors`;
//! * [`Runtime::apply_system_appearance`] — OS light/dark toggle for dual
//!   `appearance.theme` selections (CTX-0951);
//! * [`Runtime::set_font_face`] — `font.family`, `font.size`,
//!   `font.line_height`, and `font.letter_spacing` (the latter two arrive as
//!   the derived base cell) in one atlas rebuild;
//! * [`Runtime::set_outline_widths`] — the resolved `decoration.border_width*`
//!   focus/idle ring pair;
//! * [`Runtime::set_chrome_style`] — the eight global chrome-style leaves
//!   (CTX-1162 S3: `appearance.style` glow/shadow/fade plus the
//!   `decoration` tint pair, adopted as one [`ChromeStyle`](crate::ChromeStyle));
//! * [`Runtime::set_window_opacity`] — `window.opacity` on the GPU surface.
//!
//! Every setter validates fail-closed before mutating, is idempotent for an
//! unchanged value (no renderer reload, no redraw churn), and never touches
//! terminal truth (grid content, PTYs, scrollback).

use super::*;
use bitty_render::ThemePalette;

use crate::plugin_runtime::{QueuedThemeRequest, ThemePersistHandler};

/// Upper bound on a live-adopted base cell dimension in logical pixels
/// (CTX-0898).
///
/// `bitty-config` bounds the inputs (`line_height <= 2.0`,
/// `letter_spacing <= 8.0` over the legacy `8x16` base), so a real config
/// stays far below this. The bound is defense-in-depth against upstream
/// drift: an unbounded cell would let one reload request an arbitrarily
/// large glyph atlas.
pub const MAX_LIVE_CELL_PX: u32 = 256;

/// Upper bound on a live-adopted font family name in bytes, equal to the
/// `bitty-config` `font.family` bound (`MAX_FONT_FAMILY_LEN`).
///
/// Kept as a mirror (rather than naming the `bitty-config` constant
/// directly) so this live-adopt seam stays decoupled from the config
/// crate's type surface; equality is pinned by a cross-crate parity test
/// in `bitty-terminal` (the crate that depends on both), like the
/// outline-contrast floors. (`bitty-runtime` gained a normal `bitty-config`
/// dependency in CTX-0953 for `RuntimeConfig::font_fallback` validation,
/// so the mirror is now convention, not a dependency constraint.)
pub const MAX_LIVE_FONT_FAMILY_BYTES: usize = 128;

impl Runtime {
    /// Live-adopts a resolved terminal palette without restart (CTX-0898).
    ///
    /// Installs `palette` as the runtime theme and on both the renderer
    /// (default cell colors, ANSI, emitted fills) and the surface (clear
    /// color), exactly like construction (CTX-0355). Live OSC overrides the
    /// running applications set (`OSC 4` palette entries, `OSC 10/11/12`
    /// defaults) are session state, so they survive the swap: the new
    /// palette's dynamic layer is replaced by the current one and the
    /// installed fg/bg keep any active override. A change forces one full
    /// redraw; an identical base palette is a no-op.
    pub fn set_theme_palette(&mut self, palette: ThemePalette) {
        let next = ThemePalette {
            dynamic: self.config.theme.dynamic,
            ..palette
        };
        if next == self.config.theme {
            return;
        }
        self.config.theme = next;
        let installed = ThemePalette {
            foreground: self.active_foreground(),
            background: self.active_background(),
            ..self.config.theme
        };
        self.renderer.set_theme_palette(installed);
        self.surface.set_theme_palette(installed);
        self.pending_full_redraw = true;
    }

    /// Live-installs the paint-time BiDi policy (`terminal.bidi`, CTX-1612).
    ///
    /// Terminal Truth is untouched: the renderer reorders rows at paint
    /// time, so a toggle only needs one full redraw to replace retained
    /// leaf lists painted under the previous policy.
    pub fn set_bidi(&mut self, enabled: bool) {
        if self.config.bidi == enabled {
            return;
        }
        self.config.bidi = enabled;
        self.renderer.set_bidi(enabled);
        self.pending_full_redraw = true;
    }

    /// Live-swaps the palette for an OS appearance change (CTX-0951, issue
    /// #1669).
    ///
    /// `effective` is the current merged config (owns the raw
    /// `appearance.theme` selection); `appearance` is the OS signal —
    /// [`bitty_platform::SystemAppearance::Light`] swaps to the `light:`
    /// half, `Dark`/`Unknown` to the `dark:` half (dark-first, matching
    /// config resolution). Only a dual selection
    /// (`light:<name>,dark:<name>`) can change anything: the matching half
    /// resolves through [`EffectiveConfig::effective_theme_for`](bitty_config::types::EffectiveConfig::effective_theme_for)
    /// (custom `appearance.colors` overlay included) and installs via
    /// [`Self::set_theme_palette`], so dedup, one full redraw, and OSC
    /// override preservation all apply, and a toggle to the already-active
    /// half is a no-op. Single selections, unset themes, and malformed
    /// values leave the runtime untouched and return `false`; a real swap
    /// returns `true`.
    ///
    /// The embedder calls this from its
    /// `PlatformEvent::SystemAppearanceChanged` handler with the live effective
    /// config; startup and config reload keep flowing through the existing
    /// `set_theme_palette` path.
    pub fn apply_system_appearance(
        &mut self,
        effective: &bitty_config::types::EffectiveConfig,
        appearance: bitty_platform::SystemAppearance,
    ) -> bool {
        let dual = matches!(
            effective
                .appearance
                .theme
                .as_deref()
                .and_then(|raw| bitty_config::theme::parse_theme_selection(raw).ok()),
            Some(bitty_config::theme::ThemeSelection::Dual { .. })
        );
        if !dual {
            return false;
        }
        let prefer_light = matches!(appearance, bitty_platform::SystemAppearance::Light);
        let theme = effective.effective_theme_for(prefer_light);
        let before = self.config.theme;
        self.set_theme_palette(ThemePalette::from_theme(&theme));
        before != self.config.theme
    }

    /// Applies one drained colorscheme intent (CTX-1169, issue #1976).
    ///
    /// Previews (`persist = false`) live-apply through
    /// [`Self::set_theme_palette`] with no durable write, so a preview can
    /// never persist. Confirms (`persist = true`) durably commit through
    /// `persist` first and live-apply only on commit success, so a failed or
    /// absent commit leaves both the file and the screen untouched (never a
    /// silent rewrite, never a screen/file divergence). OSC overrides the
    /// running applications set survive either path via the `set_theme_palette`
    /// contract. Returns whether the live palette changed.
    ///
    /// # Errors
    ///
    /// The persist backend's error for a refused commit; nothing is applied.
    /// A missing backend (`None`) is a refused commit with state untouched.
    pub fn apply_theme_request(
        &mut self,
        request: &QueuedThemeRequest,
        persist: &Option<ThemePersistHandler>,
    ) -> Result<bool, bitty_lua::BridgeError> {
        if request.persist {
            let backend = persist
                .clone()
                .ok_or_else(|| bitty_lua::BridgeError::not_implemented("bitty.theme.confirm"))?;
            backend(&request.palette)?;
        }
        let before = self.config.theme;
        self.set_theme_palette(ThemePalette::from_custom(&request.palette));
        Ok(before != self.config.theme)
    }

    /// Live-adopts a font family, point size, and base cell without restart
    /// (CTX-0898).
    ///
    /// `cell_width`/`cell_height` are the design (scale 1.0) cell derived
    /// from `font.line_height` / `font.letter_spacing`. Family, size, and cell
    /// are adopted together so a reload that changes several of them rebuilds
    /// the glyph atlas exactly once. The renderer reloads the face at the
    /// live DPI scale first; only when that succeeds is the new state
    /// committed and the grid reflowed from the current surface extent (same
    /// path as [`Self::set_font_size`]). The window keeps its size; the grid
    /// absorbs the new cell.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] for an empty family or one longer than
    /// [`MAX_LIVE_FONT_FAMILY_BYTES`], a cell outside `1..=`[`MAX_LIVE_CELL_PX`],
    /// or a size outside the [`Self::set_font_size`] range;
    /// [`RuntimeError::Render`] when the face fails to load. Every error
    /// leaves the runtime unchanged.
    pub fn set_font_face(
        &mut self,
        family: &str,
        point_size: f32,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(), RuntimeError> {
        if family.trim().is_empty() || family.len() > MAX_LIVE_FONT_FAMILY_BYTES {
            return Err(RuntimeError::InvalidConfig(
                "font_family must be non-empty and at most MAX_LIVE_FONT_FAMILY_BYTES bytes",
            ));
        }
        if !(1..=MAX_LIVE_CELL_PX).contains(&cell_width)
            || !(1..=MAX_LIVE_CELL_PX).contains(&cell_height)
        {
            return Err(RuntimeError::InvalidConfig(
                "cell metrics must be within [1, MAX_LIVE_CELL_PX] logical pixels",
            ));
        }
        if family == self.config.font_family
            && (point_size - self.config.font_size).abs() < f32::EPSILON
            && cell_width == self.config.cell_width
            && cell_height == self.config.cell_height
        {
            return Ok(());
        }
        if !(point_size.is_finite()
            && (crate::config::FONT_ZOOM_MIN_PT..=crate::config::FONT_ZOOM_MAX_PT)
                .contains(&point_size))
        {
            return Err(RuntimeError::InvalidConfig(
                "font_size must be finite within [FONT_ZOOM_MIN_PT, FONT_ZOOM_MAX_PT]",
            ));
        }
        let base_cell = CellMetrics::new(cell_width, cell_height).map_err(RuntimeError::from)?;
        let query = FontQuery {
            family: family.to_string(),
            style: FontStyle::Normal,
            point_size,
        };
        // Load before committing: the renderer is unchanged on error, so a
        // missing face keeps the running family, size, and cell (fail-safe).
        self.renderer
            .apply_dpi_scale(base_cell, &query, self.scale_factor.get())
            .map_err(RuntimeError::from)?;
        self.config.font_family = query.family;
        self.config.font_size = point_size;
        self.config.cell_width = cell_width;
        self.config.cell_height = cell_height;
        if let Some(extent) = self
            .surface
            .extent()
            .and_then(bitty_platform::map_resize_to_surface_extent)
        {
            let (cols, rows) = self.grid_from_physical(extent);
            // Same absorb rule as `apply_dpi_scale`: the adopted renderer
            // cell always wins over stranding the window on a reflow error.
            let _ = self.reflow_to_grid(cols, rows, extent);
        }
        self.pending_full_redraw = true;
        Ok(())
    }

    /// Live-adopts the resolved focus/idle outline ring widths (CTX-0898).
    ///
    /// The pair comes from `decoration.border` -> `decoration.border_width`
    /// -> the explicit focused/idle members (CTX-0344). Paint-only: content
    /// geometry is owned by [`Self::set_decoration`]. Callers adopt the
    /// widths before [`Self::set_background_appearance`] so the per-`View`
    /// AC-2 width-cue check runs against them.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] when either width exceeds
    /// [`crate::config::MAX_OUTLINE_WIDTH_PX`]; nothing is changed.
    pub fn set_outline_widths(
        &mut self,
        focused: Option<u32>,
        idle: Option<u32>,
    ) -> Result<(), RuntimeError> {
        if [focused, idle]
            .iter()
            .flatten()
            .any(|w| *w > crate::config::MAX_OUTLINE_WIDTH_PX)
        {
            return Err(RuntimeError::InvalidConfig(
                "outline width must be within [0, MAX_OUTLINE_WIDTH_PX] logical pixels",
            ));
        }
        if self.config.outline_width_focused != focused || self.config.outline_width_idle != idle {
            self.config.outline_width_focused = focused;
            self.config.outline_width_idle = idle;
            self.pending_full_redraw = true;
        }
        Ok(())
    }

    /// Live-adopts the resolved global chrome style without restart (CTX-1162
    /// S3, DEC-0111 section 4 as corrected by DEC-0113).
    ///
    /// Covers the eight global-only style leaves (`appearance.style`
    /// glow/shadow/fade plus the `decoration` tint pair) as one
    /// [`ChromeStyle`](crate::ChromeStyle). The per-`View` glow/tint quartet
    /// rides `view_appearance` through [`Self::set_background_appearance`];
    /// callers adopt the chrome style after it, mirroring the
    /// `set_outline_widths`-before-`set_background_appearance` order. A
    /// change forces one full redraw so the new paint appears on the next
    /// present; leaves whose fade factor moved re-render through the
    /// per-leaf retained-fade gate (bounded to the visible set), and an
    /// unchanged value is a no-op. Paint-only: content geometry is untouched.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] for a glow width outside `0..=16`, a
    /// shadow offset outside `-8..=8`, or a non-finite or out-of-range fade
    /// or tint strength (the [`RuntimeConfig::validate`](crate::config::RuntimeConfig::validate)
    /// style bounds, mirrored so a direct caller cannot arm hostile paint);
    /// nothing is changed.
    pub fn set_chrome_style(
        &mut self,
        style: crate::config::ChromeStyle,
    ) -> Result<(), RuntimeError> {
        if style.glow_width > crate::config::MAX_STYLE_GLOW_WIDTH_PX {
            return Err(RuntimeError::InvalidConfig(
                "style glow width must be within [0, 16] logical pixels",
            ));
        }
        for offset in [style.shadow_offset_x, style.shadow_offset_y] {
            if offset.abs() > crate::config::MAX_STYLE_SHADOW_OFFSET_PX {
                return Err(RuntimeError::InvalidConfig(
                    "style shadow offset must be within [-8, 8] logical pixels",
                ));
            }
        }
        if !(style.fade_unfocused.is_finite() && (0.0..=1.0).contains(&style.fade_unfocused)) {
            return Err(RuntimeError::InvalidConfig(
                "style fade_unfocused must be finite within [0.0, 1.0]",
            ));
        }
        if !(style.background_tint_strength.is_finite()
            && (0.0..=1.0).contains(&style.background_tint_strength))
        {
            return Err(RuntimeError::InvalidConfig(
                "background tint strength must be finite within [0.0, 1.0]",
            ));
        }
        if self.config.chrome_style() != style {
            self.config.style_glow_color = style.glow_color;
            self.config.style_glow_width = style.glow_width;
            self.config.style_shadow_color = style.shadow_color;
            self.config.style_shadow_offset_x = style.shadow_offset_x;
            self.config.style_shadow_offset_y = style.shadow_offset_y;
            self.config.style_fade_unfocused = style.fade_unfocused;
            self.config.background_tint = style.background_tint;
            self.config.background_tint_strength = style.background_tint_strength;
            self.pending_full_redraw = true;
        }
        Ok(())
    }

    /// Requested window opacity on the attached surface (`1.0` = opaque),
    /// sanitized.
    #[must_use]
    pub fn window_opacity(&self) -> f32 {
        self.surface.opacity()
    }

    /// Live-adopts the renderer half of `window.opacity` (CTX-0898).
    ///
    /// With a GPU attached the swap chain is reconfigured through
    /// `Surface::configure_with_opacity`, which re-picks the alpha mode for
    /// the new value; headlessly (or before the first configure) the
    /// sanitized value is stored and the CPU compositor honors it on the next
    /// present. Returns whether the surface can blend the value; `false`
    /// means the platform offers no premultiplied compositing and the
    /// renderer stays opaque (fail-soft, same rule as GPU attach). The
    /// platform window's transparency hint is the embedder's half.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Render`] when the GPU surface rejects the
    /// reconfiguration.
    pub fn set_window_opacity(&mut self, opacity: f32) -> Result<bool, RuntimeError> {
        let sanitized = bitty_platform::sanitize_opacity(opacity);
        if (sanitized - self.surface.opacity()).abs() < f32::EPSILON {
            return Ok(self.surface.opacity_alpha_supported());
        }
        match (self.gpu.as_ref(), self.surface.extent()) {
            (Some(gpu), Some(extent)) if !self.surface.is_headless() => {
                self.surface
                    .configure_with_opacity(gpu, extent, sanitized)
                    .map_err(RuntimeError::from)?;
            }
            _ => self.surface.set_opacity(sanitized),
        }
        self.pending_full_redraw = true;
        Ok(self.surface.opacity_alpha_supported())
    }
}

#[cfg(test)]
mod tests {
    use crate::Runtime;
    use crate::config::RuntimeConfig;

    fn light() -> crate::ThemePalette {
        crate::ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("github-light")))
    }

    #[test]
    fn theme_palette_swaps_live_and_keeps_osc_overrides() {
        let mut rt = Runtime::with_defaults().expect("runtime");
        rt.apply_osc_palette(3, [1, 2, 3]);
        rt.apply_osc_color(bitty_vt::DynamicColorTarget::Foreground, [9, 9, 9]);
        let _ = rt.tick();
        rt.set_theme_palette(light());
        assert_eq!(rt.config().theme.background, light().background);
        assert_eq!(rt.active_background(), light().background);
        assert_eq!(rt.active_palette_color(3), [1, 2, 3], "OSC 4 survives");
        assert_eq!(rt.active_foreground(), [9, 9, 9, 0xFF], "OSC 10 survives");
        assert_eq!(rt.surface.theme_palette().background, light().background);
        assert!(rt.tick().is_some(), "a theme change repaints");
    }

    #[test]
    fn identical_theme_palette_is_a_no_op() {
        let mut rt = Runtime::with_defaults().expect("runtime");
        let _ = rt.tick();
        let current = rt.config().theme;
        rt.set_theme_palette(current);
        assert!(rt.tick().is_none(), "no churn for an unchanged palette");
    }

    fn dual_effective() -> bitty_config::types::EffectiveConfig {
        bitty_config::types::EffectiveConfig {
            appearance: bitty_config::types::AppearanceConfig {
                theme: Some("light:github-light,dark:dracula".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn system_appearance_swaps_dual_theme_live() {
        use bitty_platform::SystemAppearance;
        let mut rt = Runtime::with_defaults().expect("runtime");
        let effective = dual_effective();
        let _ = rt.tick();
        // Unknown OS appearance resolves dark-first.
        assert!(rt.apply_system_appearance(&effective, SystemAppearance::Unknown));
        let dracula =
            crate::ThemePalette::from_theme(bitty_config::theme::resolve_theme(Some("dracula")));
        assert_eq!(rt.config().theme.background, dracula.background);
        assert!(rt.tick().is_some(), "the first swap repaints");
        // The already-active half is a no-op (set_theme_palette dedup).
        assert!(!rt.apply_system_appearance(&effective, SystemAppearance::Dark));
        assert!(rt.tick().is_none(), "no churn for the active half");
        // A light toggle swaps the live palette and repaints.
        assert!(rt.apply_system_appearance(&effective, SystemAppearance::Light));
        assert_eq!(rt.config().theme.background, light().background);
        assert!(rt.tick().is_some(), "an appearance swap repaints");
    }

    #[test]
    fn system_appearance_ignores_single_and_unset_themes() {
        use bitty_platform::SystemAppearance;
        let mut rt = Runtime::with_defaults().expect("runtime");
        let _ = rt.tick();
        let before = rt.config().theme;
        let mut single = dual_effective();
        single.appearance.theme = Some("dracula".to_string());
        assert!(!rt.apply_system_appearance(&single, SystemAppearance::Light));
        let mut unset = dual_effective();
        unset.appearance.theme = None;
        assert!(!rt.apply_system_appearance(&unset, SystemAppearance::Light));
        let mut malformed = dual_effective();
        malformed.appearance.theme = Some("light:".to_string());
        assert!(!rt.apply_system_appearance(&malformed, SystemAppearance::Light));
        assert_eq!(rt.config().theme, before);
        assert!(rt.tick().is_none(), "ignored toggles never repaint");
    }

    #[test]
    fn font_face_adopts_family_and_cell_and_reflows() {
        let mut rt =
            Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("runtime");
        let (cols, rows) = (rt.snapshot().width, rt.snapshot().height);
        let width = rt.config().cell_width;
        let height = rt.config().cell_height;
        let size = rt.config().font_size;
        rt.set_font_face("Alt Mono", size, width + 2, height + 4)
            .expect("adopt");
        assert_eq!(rt.config().font_family, "Alt Mono");
        assert_eq!(rt.live_cell_size(), (width + 2, height + 4));
        let snap = rt.snapshot();
        assert!(
            snap.width < cols && snap.height < rows,
            "a wider/taller cell reflows to fewer cells ({cols}x{rows} -> {}x{})",
            snap.width,
            snap.height
        );
    }

    #[test]
    fn font_face_rejects_bad_input_without_mutation() {
        let mut rt =
            Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("runtime");
        let before = rt.config().clone();
        let size = before.font_size;
        assert!(rt.set_font_face("  ", size, 10, 22).is_err());
        let long = "x".repeat(super::MAX_LIVE_FONT_FAMILY_BYTES + 1);
        assert!(rt.set_font_face(&long, size, 10, 22).is_err());
        assert!(rt.set_font_face("Mono", size, 0, 22).is_err());
        assert!(
            rt.set_font_face("Mono", size, 10, super::MAX_LIVE_CELL_PX + 1)
                .is_err()
        );
        assert!(rt.set_font_face("Mono", f32::NAN, 10, 22).is_err());
        assert!(
            rt.set_font_face("Mono", crate::config::FONT_ZOOM_MAX_PT + 1.0, 10, 22)
                .is_err()
        );
        assert_eq!(rt.config(), &before);
    }

    #[test]
    fn font_face_adopts_size_in_the_same_rebuild() {
        let mut rt =
            Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("runtime");
        let (width, height) = (rt.config().cell_width, rt.config().cell_height);
        rt.set_font_face("Alt Mono", 18.0, width, height)
            .expect("adopt");
        assert!((rt.font_size() - 18.0).abs() < f32::EPSILON);
        assert_eq!(rt.config().font_family, "Alt Mono");
        // The size is already adopted, so the follow-up size setter is a
        // no-op (no second atlas rebuild).
        let _ = rt.tick();
        rt.set_font_size(18.0).expect("same size");
        assert!(rt.tick().is_none(), "no second rebuild or redraw");
    }

    #[test]
    fn outline_widths_adopt_and_reject_out_of_bound() {
        let mut rt = Runtime::with_defaults().expect("runtime");
        rt.set_outline_widths(Some(3), Some(1)).expect("adopt");
        assert_eq!(rt.config().outline_width_focused, Some(3));
        assert_eq!(rt.config().outline_width_idle, Some(1));
        let over = crate::config::MAX_OUTLINE_WIDTH_PX + 1;
        assert!(rt.set_outline_widths(Some(over), None).is_err());
        assert_eq!(rt.config().outline_width_focused, Some(3), "unchanged");
    }

    #[test]
    fn chrome_style_adopts_live_repaints_once_and_rejects_bad_input() {
        use crate::config::ChromeStyle;
        let mut rt = Runtime::with_defaults().expect("runtime");
        let _ = rt.tick().expect("first tick presents");
        assert_eq!(rt.tick(), None, "idle on unchanged frame");
        assert!(rt.config().chrome_style().is_inert());
        let styled = ChromeStyle {
            glow_color: Some([0xFF, 0x00, 0x00, 0xFF]),
            glow_width: 4,
            shadow_color: Some([0x00, 0x00, 0x00, 0x80]),
            shadow_offset_x: 3,
            shadow_offset_y: -3,
            fade_unfocused: 0.5,
            background_tint: Some([0x11, 0x22, 0x33, 0xFF]),
            background_tint_strength: 0.25,
        };
        assert!(!styled.is_inert());
        rt.set_chrome_style(styled).expect("adopt");
        assert_eq!(rt.config().chrome_style(), styled);
        assert!(rt.tick().is_some(), "a chrome style change repaints");
        assert_eq!(rt.tick(), None, "must idle after one repaint");
        // A no-op set does not force another frame.
        rt.set_chrome_style(styled).expect("same value valid");
        assert_eq!(rt.tick(), None, "no-op set must not repaint");
        // Out-of-bound values fail closed and retain the previous value.
        let over_glow = ChromeStyle {
            glow_width: crate::config::MAX_STYLE_GLOW_WIDTH_PX + 1,
            ..styled
        };
        assert!(rt.set_chrome_style(over_glow).is_err());
        let over_shadow = ChromeStyle {
            shadow_offset_x: crate::config::MAX_STYLE_SHADOW_OFFSET_PX + 1,
            ..styled
        };
        assert!(rt.set_chrome_style(over_shadow).is_err());
        let bad_fade = ChromeStyle {
            fade_unfocused: f32::NAN,
            ..styled
        };
        assert!(rt.set_chrome_style(bad_fade).is_err());
        let bad_strength = ChromeStyle {
            background_tint_strength: 2.0,
            ..styled
        };
        assert!(rt.set_chrome_style(bad_strength).is_err());
        assert_eq!(rt.config().chrome_style(), styled, "unchanged");
    }

    #[test]
    fn window_opacity_adopts_on_headless_surface() {
        let mut rt = Runtime::with_defaults().expect("runtime");
        assert!((rt.window_opacity() - 1.0).abs() < f32::EPSILON);
        assert!(rt.set_window_opacity(0.5).expect("adopt"));
        assert!((rt.window_opacity() - 0.5).abs() < f32::EPSILON);
        // Non-finite input degrades to opaque instead of poisoning output.
        assert!(rt.set_window_opacity(f32::NAN).expect("sanitized"));
        assert!((rt.window_opacity() - 1.0).abs() < f32::EPSILON);
    }

    fn custom_palette() -> bitty_config::theme::CustomPalette {
        let ansi = vec!["#45475a".to_string(); 16];
        bitty_config::theme::CustomPalette::from_hex(
            "#1e1e2e", "#cdd6f4", "#f5e0dc", "#313244", &ansi,
        )
        .expect("test palette parses")
    }

    fn theme_request(persist: bool) -> crate::plugin_runtime::QueuedThemeRequest {
        crate::plugin_runtime::QueuedThemeRequest {
            palette: custom_palette(),
            persist,
        }
    }

    #[test]
    fn theme_preview_applies_live_without_persist() {
        use std::cell::RefCell;
        use std::rc::Rc;
        // CTX-1169: a preview live-applies through `set_theme_palette` and
        // never touches the persist backend, even when one is wired.
        let mut rt = Runtime::with_defaults().expect("runtime");
        let calls = Rc::new(RefCell::new(0u32));
        let counting_calls = Rc::clone(&calls);
        let backend: crate::plugin_runtime::ThemePersistHandler = Rc::new(move |_| {
            *counting_calls.borrow_mut() += 1;
            Ok(())
        });
        let changed = rt
            .apply_theme_request(&theme_request(false), &Some(backend))
            .expect("preview applies");
        assert!(changed, "a new palette repaints");
        assert_eq!(
            rt.config().theme,
            crate::ThemePalette::from_custom(&custom_palette()),
            "preview installs the palette live"
        );
        assert_eq!(*calls.borrow(), 0, "preview never persists");
        assert!(rt.tick().is_some(), "a preview repaints");
        // Repeating the preview is a no-op through the set_palette dedup.
        let changed = rt
            .apply_theme_request(&theme_request(false), &None)
            .expect("repeat previews");
        assert!(!changed, "an identical preview changes nothing");
    }

    #[test]
    fn theme_confirm_persists_before_applying() {
        use std::cell::RefCell;
        use std::rc::Rc;
        // CTX-1169: a confirm commits through the backend first; the recorded
        // palette is the applied one, so screen and file cannot diverge.
        let mut rt = Runtime::with_defaults().expect("runtime");
        let committed = Rc::new(RefCell::new(Vec::new()));
        let recording = Rc::clone(&committed);
        let backend: crate::plugin_runtime::ThemePersistHandler = Rc::new(move |palette| {
            recording.borrow_mut().push(*palette);
            Ok(())
        });
        let changed = rt
            .apply_theme_request(&theme_request(true), &Some(backend))
            .expect("confirm applies");
        assert!(changed, "a new palette repaints");
        assert_eq!(committed.borrow().len(), 1, "confirm persists once");
        assert_eq!(committed.borrow()[0], custom_palette());
        assert_eq!(
            rt.config().theme,
            crate::ThemePalette::from_custom(&custom_palette())
        );
    }

    #[test]
    fn theme_confirm_without_backend_leaves_state_untouched() {
        // CTX-1169: a confirm with no persist backend fails closed with
        // state untouched (never a silent live-only rewrite).
        let mut rt = Runtime::with_defaults().expect("runtime");
        let _ = rt.tick();
        let before = rt.config().theme;
        let error = rt
            .apply_theme_request(&theme_request(true), &None)
            .expect_err("missing backend refuses");
        assert_eq!(error.code, "E_NOT_IMPLEMENTED");
        assert_eq!(rt.config().theme, before, "state untouched");
        assert!(rt.tick().is_none(), "no repaint for a refused confirm");
    }

    #[test]
    fn theme_confirm_with_failing_backend_leaves_state_untouched() {
        use std::rc::Rc;
        // CTX-1169: a refused durable commit blocks the live-apply, so a
        // failed confirm leaves both the file and the screen untouched.
        let mut rt = Runtime::with_defaults().expect("runtime");
        let _ = rt.tick();
        let before = rt.config().theme;
        let backend: crate::plugin_runtime::ThemePersistHandler =
            Rc::new(|_| Err(bitty_lua::BridgeError::new("io", "E_IO", "disk refused")));
        let error = rt
            .apply_theme_request(&theme_request(true), &Some(backend))
            .expect_err("failed commit refuses");
        assert_eq!(error.code, "E_IO");
        assert_eq!(rt.config().theme, before, "state untouched");
        assert!(rt.tick().is_none(), "no repaint for a refused confirm");
    }
}
