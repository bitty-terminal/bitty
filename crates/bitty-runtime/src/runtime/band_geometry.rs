//! Shared paint and hit-test geometry for plugin bands. Every band occupies
//! ordinary window cells, including the bottom stack. The window-grid origin
//! distributes the vertical remainder outside the stack (CTX-1922).

use super::Runtime;

/// Physical, window-relative pixel rectangle of a chrome band or span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BandPixels {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl BandPixels {
    /// Whether `(x, y)` falls inside the half-open painted rectangle.
    pub(super) fn contains(&self, x: f64, y: f64) -> bool {
        let left = f64::from(self.x);
        let top = f64::from(self.y);
        let right = left + f64::from(self.width);
        let bottom = top + f64::from(self.height);
        x >= left && x < right && y >= top && y < bottom
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BandPixelGeometry {
    origin: (i32, i32),
    cell_w: u32,
    cell_h: u32,
}

impl BandPixelGeometry {
    /// Maps a granted rectangle, in cells, to window pixels.
    pub(super) fn rect(
        &self,
        row: u16,
        column: u16,
        width_cells: u16,
        height_cells: u16,
    ) -> BandPixels {
        let span = |cells, cell| i64::from(cells) * i64::from(cell);
        let x = i64::from(self.origin.0) + span(column, self.cell_w);
        let y = i64::from(self.origin.1) + span(row, self.cell_h);
        BandPixels {
            x: i32::try_from(x).unwrap_or(i32::MAX),
            y: i32::try_from(y).unwrap_or(i32::MAX),
            width: u32::try_from(span(width_cells, self.cell_w)).unwrap_or(u32::MAX),
            height: u32::try_from(span(height_cells, self.cell_h)).unwrap_or(u32::MAX),
        }
    }
}

impl Runtime {
    pub(super) fn band_pixel_geometry(&self, origin: (i32, i32)) -> BandPixelGeometry {
        let live = self.live_cell_metrics();
        BandPixelGeometry {
            origin,
            cell_w: live.width,
            cell_h: live.height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_share_the_window_cell_mapping() {
        let geo = BandPixelGeometry {
            origin: (13, 17),
            cell_w: 14,
            cell_h: 30,
        };
        let top = geo.rect(0, 0, 110, 1);
        let side = geo.rect(1, 4, 2, 27);
        let inner = geo.rect(28, 0, 110, 1);
        let outer = geo.rect(29, 0, 110, 1);
        assert_eq!((top.x, top.y), (13, 17));
        assert_eq!((side.x, side.y, side.height), (69, 47, 810));
        assert_eq!(side.y + side.height as i32, inner.y);
        assert_eq!((inner.y, inner.height), (857, 30));
        assert_eq!((outer.y, outer.height), (887, 30));
    }

    #[test]
    fn contains_is_half_open_on_both_axes() {
        let rect = BandPixels {
            x: 10,
            y: 20,
            width: 30,
            height: 19,
        };
        assert!(rect.contains(10.0, 20.0));
        assert!(rect.contains(39.9, 38.9));
        assert!(!rect.contains(40.0, 30.0));
        assert!(!rect.contains(20.0, 39.0));
        assert!(!rect.contains(9.9, 30.0));
    }
}
