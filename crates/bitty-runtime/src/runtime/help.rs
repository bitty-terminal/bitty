//! `Runtime` — Help popup overlay state and paint (CTX-0265).
//!
//! The which-key help panel (009 §which-key, DEC-0035 follow-through):
//! a floating overlay listing every bound shortcut, generated FROM the
//! live keymap registry
//! ([`bitty_config::keymap::help_rows_from_keymaps`]) — never a hardcoded
//! copy. Toggled by the `toggle_help` chrome action (`Mod+backtick` /
//! `Mod+?`), dismissed by `Esc` or the same chord.
//!
//! Overlay-tier rules (panel pre-study §overlay, CTX-0217 tiers): the popup
//! is presentation-only — [`Runtime::paint_help_panel`] pushes fills and
//! glyphs onto the presented frame and never touches grid cells,
//! scrollback, layout, or focus. It is informational, not modal: while
//! visible, other bound chords still dispatch and unbound keys still reach
//! the shell (it never feeds `modal_capture_active`).
use super::*;

/// Title line painted at the top of the help panel.
pub const HELP_PANEL_TITLE: &str = "Keyboard shortcuts";
/// Footer line painted at the bottom of the help panel (ASCII-only so the
/// overlay paints under every font stack, including headless).
pub const HELP_PANEL_FOOTER: &str = "Esc closes | repeat toggle closes";
/// Maximum stored help rows (CTX-0265 bound, threat T-01).
///
/// The resolved table caps user entries at
/// [`bitty_config::types::MAX_KEYMAPS`]; rows beyond this cap are dropped
/// oldest-last (defaults sort first by identity, so shipped rows survive).
/// Each row caps at `MAX_CHORD_LEN + MAX_ACTION_LEN + 2` bytes by
/// construction, so worst-case retention is bounded (~33 KiB).
pub const HELP_MAX_ROWS: usize = 256;
/// Minimum panel inner width in cells (below this the view is too narrow
/// to list anything legible, so the paint is skipped).
const HELP_MIN_INNER: u16 = 10;

/// Resolved help-panel geometry in view cells (CTX-0265).
///
/// Pure over integers so panel placement is headless-testable without a
/// renderer: `x`/`y`/`w`/`h` cover the border, `visible_rows` is how many
/// registry rows fit, and `overflow` is the hidden tail shown as
/// `+N more`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HelpPanelLayout {
    /// Left column of the panel (view cells).
    pub x: u16,
    /// Top row of the panel (view cells).
    pub y: u16,
    /// Panel width in cells (including the 1-cell border).
    pub w: u16,
    /// Panel height in cells (including border, title, and footer).
    pub h: u16,
    /// Registry rows actually painted.
    pub visible_rows: usize,
    /// Registry rows hidden behind the `+N more` tail.
    pub overflow: usize,
}

/// Resolve the centered help-panel geometry for a view (CTX-0265).
///
/// Returns `None` when the view is too small to hold a legible panel
/// (narrower than the minimum inner width, or fewer than six rows): the
/// help state stays visible and a later resize paints it — never a panic,
/// never a zero-area fill. Otherwise the panel is centered with a 1-cell
/// border, one title row, the fitting registry rows (plus a `+N more`
/// tail when truncated), and one footer row.
pub(super) fn help_panel_layout(
    view_w: u16,
    view_h: u16,
    rows: &[String],
) -> Option<HelpPanelLayout> {
    if view_w < HELP_MIN_INNER + 2 || view_h < 6 {
        return None;
    }
    let content_width = HELP_PANEL_TITLE
        .chars()
        .count()
        .max(HELP_PANEL_FOOTER.chars().count())
        .max(rows.iter().map(|r| r.chars().count()).max().unwrap_or(0));
    let inner = (content_width.min(usize::from(view_w - 2)) as u16).max(HELP_MIN_INNER);
    let w = inner + 2;
    // Body budget: title + footer + borders consume 4 rows; a truncated
    // tail additionally reserves one `+N more` row.
    let body = usize::from(view_h).saturating_sub(4);
    if body == 0 {
        return None;
    }
    let (visible_rows, overflow) = if rows.len() <= body {
        (rows.len(), 0)
    } else {
        (body.saturating_sub(1).max(1), rows.len())
    };
    let overflow = if overflow > 0 {
        rows.len().saturating_sub(visible_rows)
    } else {
        0
    };
    let h = (4 + visible_rows + usize::from(overflow > 0)) as u16;
    if h > view_h {
        return None;
    }
    Some(HelpPanelLayout {
        x: view_w.saturating_sub(w) / 2,
        y: view_h.saturating_sub(h) / 2,
        w,
        h,
        visible_rows,
        overflow,
    })
}

impl Runtime {
    /// Whether the help popup is currently shown (CTX-0265).
    #[must_use]
    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// Current help rows (regenerated from the live keymap registry by
    /// the app on every show; empty until the first show).
    #[must_use]
    pub fn help_rows(&self) -> &[String] {
        &self.help_rows
    }

    /// Replace the help rows (CTX-0265).
    ///
    /// Called by the app with
    /// [`bitty_config::keymap::help_rows_from_keymaps`] output on every
    /// show, so the popup stays in sync with the live registry by
    /// construction. Truncates to [`HELP_MAX_ROWS`] (defaults sort first,
    /// so shipped rows survive); forces a repaint when visible.
    pub fn set_help_rows(&mut self, rows: Vec<String>) {
        let mut rows = rows;
        if rows.len() > HELP_MAX_ROWS {
            rows.truncate(HELP_MAX_ROWS);
        }
        self.help_rows = rows;
        if self.help_visible {
            self.pending_full_redraw = true;
        }
    }

    /// Flip help visibility (CTX-0265 `toggle_help` action target).
    ///
    /// Returns the new visibility. Forces a repaint so the frame-on-demand
    /// tick presents the transition even with no PTY damage. Never touches
    /// grid truth.
    pub fn toggle_help(&mut self) -> bool {
        self.help_visible = !self.help_visible;
        self.pending_full_redraw = true;
        self.help_visible
    }

    /// Hide the help popup without further effect (CTX-0265 `Esc` path).
    ///
    /// Returns `true` when the popup was visible (the caller consumed the
    /// key); `false` otherwise (routing untouched). Forces a repaint on a
    /// real dismissal so the panel clears on the next tick.
    pub fn dismiss_help(&mut self) -> bool {
        if !self.help_visible {
            return false;
        }
        self.help_visible = false;
        self.pending_full_redraw = true;
        true
    }

    /// Paint the help popup onto the present-layer overlay (CTX-0265).
    ///
    /// Centered floating panel on the focused view: border fill, inner
    /// background fill, title glyphs, one glyph run per fitting registry
    /// row (clipped to the panel), an optional `+N more` tail, and the
    /// footer. Overlay only — pushes fills/glyphs, never mutates cells,
    /// scrollback, layout, or focus. Returns `true` when anything was
    /// pushed (the caller marks the frame dirty); `false` when hidden,
    /// rowless, or the view is too small (state kept for a later resize).
    pub(super) fn paint_help_panel(
        &mut self,
        allocations: &[(ViewId, UiRect)],
        view_map: &std::collections::HashMap<ViewId, View>,
        pad_px: i32,
        fills: &mut Vec<bitty_render::grid::FillRect>,
        glyphs: &mut Vec<bitty_render::grid::GlyphInstance>,
    ) -> bool {
        if !self.help_visible || self.help_rows.is_empty() {
            return false;
        }
        let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) else {
            return false;
        };
        let Some((_, rect)) = allocations.iter().find(|(id, _)| *id == fid) else {
            return false;
        };
        if rect.width == 0 || rect.height == 0 {
            return false;
        }
        let Some(panel) = help_panel_layout(rect.width, rect.height, &self.help_rows) else {
            return false;
        };
        let live = self.live_cell_metrics();
        let cw = live.width as i32;
        let ch = live.height as i32;
        let origin_px_x = rect.x as i32 * cw + pad_px;
        let origin_px_y = rect.y as i32 * ch + pad_px;
        let panel_px_x = origin_px_x + panel.x as i32 * cw;
        let panel_px_y = origin_px_y + panel.y as i32 * ch;
        let panel_px_w = panel.w as u32 * live.width;
        let panel_px_h = panel.h as u32 * live.height;
        // Border: full-bleed fill, then the inset background leaves a
        // 1-cell outline.
        fills.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                panel_px_x, panel_px_y, panel_px_w, panel_px_h,
            ),
            color: bitty_render::grid::HELP_PANEL_BORDER,
        });
        fills.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                panel_px_x + cw,
                panel_px_y + ch,
                panel_px_w.saturating_sub(2 * live.width),
                panel_px_h.saturating_sub(2 * live.height),
            ),
            color: bitty_render::grid::HELP_PANEL_BG,
        });
        let inner_cells = usize::from(panel.w.saturating_sub(2));
        let text_x = panel_px_x + cw;
        let title = self.renderer.overlay_text_glyphs(
            HELP_PANEL_TITLE,
            (text_x, panel_px_y + ch),
            inner_cells,
            bitty_render::grid::HELP_PANEL_FG,
        );
        glyphs.extend(title);
        for (i, row) in self.help_rows.iter().take(panel.visible_rows).enumerate() {
            let row_glyphs = self.renderer.overlay_text_glyphs(
                row,
                (text_x, panel_px_y + (2 + i as i32) * ch),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(row_glyphs);
        }
        let mut footer_row = 2 + panel.visible_rows as i32;
        if panel.overflow > 0 {
            let tail = format!("+{} more", panel.overflow);
            let tail_glyphs = self.renderer.overlay_text_glyphs(
                &tail,
                (text_x, panel_px_y + footer_row * ch),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(tail_glyphs);
            footer_row += 1;
        }
        let footer = self.renderer.overlay_text_glyphs(
            HELP_PANEL_FOOTER,
            (text_x, panel_px_y + footer_row * ch),
            inner_cells,
            bitty_render::grid::HELP_PANEL_FG,
        );
        glyphs.extend(footer);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::help_panel_layout;

    #[test]
    fn panel_centers_with_border_title_footer() {
        let rows = vec![
            "alt+h  goto_split:left".to_string(),
            "alt+`  toggle_help".to_string(),
        ];
        let panel = help_panel_layout(80, 24, &rows).expect("fits");
        // inner = max(title 18, footer 33, rows) + 2 border.
        assert_eq!(panel.visible_rows, 2);
        assert_eq!(panel.overflow, 0);
        assert_eq!(panel.h, 4 + 2);
        assert_eq!(panel.w, 33 + 2);
        assert_eq!(panel.x, (80 - panel.w) / 2);
        assert_eq!(panel.y, (24 - panel.h) / 2);
    }

    #[test]
    fn panel_truncates_with_more_tail() {
        let rows: Vec<String> = (0..50)
            .map(|i| format!("alt+{i}  workspace_focus:1"))
            .collect();
        let panel = help_panel_layout(80, 10, &rows).expect("fits");
        // body budget 10 - 4 = 6; truncated tail reserves one row.
        assert_eq!(panel.visible_rows, 5);
        assert_eq!(panel.overflow, 45);
        assert_eq!(panel.h, 10);
    }

    #[test]
    fn panel_refuses_tiny_views() {
        let rows = vec!["alt+h  goto_split:left".to_string()];
        assert!(help_panel_layout(11, 24, &rows).is_none(), "too narrow");
        assert!(help_panel_layout(80, 5, &rows).is_none(), "too short");
        assert!(help_panel_layout(0, 0, &rows).is_none(), "empty");
    }
}
