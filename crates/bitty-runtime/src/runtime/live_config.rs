//! `Runtime` — live-adopt setters for config reload (CTX-0898, issue #1522).
//!
//! The CTX-0814 reload path adopts the presentation subset through the
//! existing setters (`set_decoration`, `set_outline`, `set_font_size`, ...).
//! This module adds the remaining runtime-owned `Live` fields:
//!
//! * [`Runtime::set_theme_palette`] — `appearance.theme` / `appearance.colors`;
//! * [`Runtime::set_font_face`] — `font.family`, `font.line_height`, and
//!   `font.letter_spacing` (the latter two arrive as the derived base cell);
//! * [`Runtime::set_window_opacity`] — `window.opacity` on the GPU surface.
//!
//! Every setter validates fail-closed before mutating, is idempotent for an
//! unchanged value (no renderer reload, no redraw churn), and never touches
//! terminal truth (grid content, PTYs, scrollback).

use super::*;
use bitty_render::ThemePalette;

/// Upper bound on a live-adopted base cell dimension in logical pixels
/// (CTX-0898).
///
/// `bitty-config` bounds the inputs (`line_height <= 2.0`,
/// `letter_spacing <= 8.0` over the legacy `8x16` base), so a real config
/// stays far below this. The bound is defense-in-depth against upstream
/// drift: an unbounded cell would let one reload request an arbitrarily
/// large glyph atlas.
pub const MAX_LIVE_CELL_PX: u32 = 256;

/// Upper bound on a live-adopted font family name in bytes (CTX-0898),
/// mirroring the `bitty-config` `font.family` bound.
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

    /// Live-adopts a font family and base cell without restart (CTX-0898).
    ///
    /// `cell_width`/`cell_height` are the design (scale 1.0) cell derived
    /// from `font.line_height` / `font.letter_spacing`. The renderer reloads
    /// the face at the live DPI scale first; only when that succeeds is the
    /// new family/cell committed and the grid reflowed from the current
    /// surface extent (same path as [`Self::set_font_size`]). The window
    /// keeps its size; the grid absorbs the new cell.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::InvalidConfig`] for an empty/oversize family or a cell
    /// outside `1..=MAX_LIVE_CELL_PX`; [`RuntimeError::Render`] when the face
    /// fails to load. Every error leaves the runtime unchanged.
    pub fn set_font_face(
        &mut self,
        family: &str,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<(), RuntimeError> {
        if family.trim().is_empty() || family.len() > MAX_LIVE_FONT_FAMILY_BYTES {
            return Err(RuntimeError::InvalidConfig(
                "font_family must be non-empty and at most 128 bytes",
            ));
        }
        if !(1..=MAX_LIVE_CELL_PX).contains(&cell_width)
            || !(1..=MAX_LIVE_CELL_PX).contains(&cell_height)
        {
            return Err(RuntimeError::InvalidConfig(
                "cell metrics must be within [1, 256] logical pixels",
            ));
        }
        if family == self.config.font_family
            && cell_width == self.config.cell_width
            && cell_height == self.config.cell_height
        {
            return Ok(());
        }
        let base_cell = CellMetrics::new(cell_width, cell_height).map_err(RuntimeError::from)?;
        let query = FontQuery {
            family: family.to_string(),
            style: FontStyle::Normal,
            point_size: self.config.font_size,
        };
        // Load before committing: the renderer is unchanged on error, so a
        // missing face keeps the running family and cell (fail-safe).
        self.renderer
            .apply_dpi_scale(base_cell, &query, self.scale_factor.get())
            .map_err(RuntimeError::from)?;
        self.config.font_family = query.family;
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

    #[test]
    fn font_face_adopts_family_and_cell_and_reflows() {
        let mut rt =
            Runtime::with_deterministic_rasterizer(RuntimeConfig::default()).expect("runtime");
        let (cols, rows) = (rt.snapshot().width, rt.snapshot().height);
        let width = rt.config().cell_width;
        let height = rt.config().cell_height;
        rt.set_font_face("Alt Mono", width + 2, height + 4)
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
        assert!(rt.set_font_face("  ", 10, 22).is_err());
        assert!(rt.set_font_face(&"x".repeat(129), 10, 22).is_err());
        assert!(rt.set_font_face("Mono", 0, 22).is_err());
        assert!(
            rt.set_font_face("Mono", 10, super::MAX_LIVE_CELL_PX + 1)
                .is_err()
        );
        assert_eq!(rt.config(), &before);
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
}
