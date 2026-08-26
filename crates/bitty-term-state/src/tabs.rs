//! Tab-stop lattice over the grid columns.
//!
//! RFC "Grid and state invariants", invariant 6: tab stops lie within
//! `[0, width)` and `FullReset` restores the default tab lattice. The
//! lattice is a per-column boolean vector; [`TabStops::default_lattice`]
//! places stops every [`crate::DEFAULT_TAB_INTERVAL`] columns.

/// Boolean tab-stop vector, one bit per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStops {
    bits: Vec<bool>,
}

impl TabStops {
    /// Stops for `columns` columns with no stops set.
    #[must_use]
    pub fn empty(columns: usize) -> Self {
        Self {
            bits: vec![false; columns],
        }
    }

    /// The default lattice: a stop at every
    /// [`crate::DEFAULT_TAB_INTERVAL`] columns within `[0, columns)`.
    #[must_use]
    pub fn default_lattice(columns: usize) -> Self {
        let mut stops = Self::empty(columns);
        for col in (crate::DEFAULT_TAB_INTERVAL..columns).step_by(crate::DEFAULT_TAB_INTERVAL) {
            stops.bits[col] = true;
        }
        stops
    }

    /// Number of columns covered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    /// Whether the lattice covers no columns at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    /// Whether a stop exists at `col`.
    #[must_use]
    pub fn contains(&self, col: usize) -> bool {
        self.bits.get(col).copied().unwrap_or(false)
    }

    /// Sets a stop at `col`; out-of-range columns are ignored so every stop
    /// stays within `[0, width)` (RFC invariant 6).
    pub fn set(&mut self, col: usize) {
        if let Some(slot) = self.bits.get_mut(col) {
            *slot = true;
        }
    }

    /// Clears the stop at `col`, if any.
    pub fn clear_at(&mut self, col: usize) {
        if let Some(slot) = self.bits.get_mut(col) {
            *slot = false;
        }
    }

    /// Clears every stop.
    pub fn clear_all(&mut self) {
        for slot in &mut self.bits {
            *slot = false;
        }
    }

    /// The next stop strictly after `col`, or `None` when no stop remains.
    #[must_use]
    pub fn next_after(&self, col: usize) -> Option<usize> {
        ((col + 1)..self.bits.len()).find(|&c| self.bits[c])
    }

    /// The previous stop strictly before `col`, or `None`.
    #[must_use]
    pub fn prev_before(&self, col: usize) -> Option<usize> {
        if col == 0 {
            return None;
        }
        (0..col).rev().find(|&c| self.bits[c])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_lattice_places_stops_every_interval() {
        let stops = TabStops::default_lattice(40);
        assert!(stops.contains(8));
        assert!(stops.contains(16));
        assert!(stops.contains(32));
        assert!(!stops.contains(0));
        assert!(!stops.contains(39));
        assert_eq!(stops.len(), 40);
    }

    #[test]
    fn set_ignores_out_of_range_columns() {
        let mut stops = TabStops::empty(4);
        stops.set(2);
        stops.set(100);
        assert!(stops.contains(2));
        assert!(!stops.contains(3));
    }

    #[test]
    fn next_and_prev_scan_the_lattice() {
        let mut stops = TabStops::empty(20);
        stops.set(5);
        stops.set(12);
        assert_eq!(stops.next_after(0), Some(5));
        assert_eq!(stops.next_after(5), Some(12));
        assert_eq!(stops.next_after(12), None);
        assert_eq!(stops.prev_before(12), Some(5));
        assert_eq!(stops.prev_before(5), None);
    }

    #[test]
    fn clear_all_removes_every_stop() {
        let mut stops = TabStops::default_lattice(32);
        stops.clear_all();
        assert!((0..32).all(|c| !stops.contains(c)));
    }
}
