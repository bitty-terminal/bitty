//! Kitty graphics placements: grid-anchored display records (CTX-0950).
//!
//! The parser (`bitty-vt`) turns `APC G` bytes into [`bitty_vt::TerminalAction::KittyGraphics`];
//! this module is the terminal-state half of the advanced subset (issue
//! #1668): where images appear on (or under) the text grid, how they move
//! when the grid scrolls, when they die, and how multi-frame animations
//! advance — all headless, bounded, and deterministic.
//!
//! Reference semantics (kitty `graphics-protocol.rst`, ghostty
//! `graphics_command`/`graphics_storage`/`graphics_animation`, kitty
//! `graphics.c`):
//!
//! - Every transmitted image may be displayed any number of times; each
//!   display is a *placement* keyed by `(image id, placement id)`.
//!   Re-placing the same pair replaces the old placement (no flicker);
//!   `p=0` (or absent) with a non-zero image id creates an additional
//!   placement; a placement id on image `0` is ignored (anonymous).
//! - Virtual placements (`U=1`) are invisible prototypes for `U+10EEEE`
//!   runs (decoded in [`crate::kitty_unicode`]): they carry a cell span
//!   but no screen anchor, never move the cursor, and are deleted only by
//!   the `i`/`I`/`r`/`R`/`n`/`N` selectors — never by cell/row/column/z
//!   selectors, which address physical screen locations.
//! - Relative placements (`P=`/`Q=` with `H=`/`V=` offsets) anchor to a
//!   parent placement; a missing parent refuses the placement
//!   (`ENOPARENT`), a virtual prototype as a child is refused, deletion
//!   of the parent cascades, and the cursor never moves for them.
//! - Animation: frame 1 is the root (image base data); `a=f` appends
//!   (`r=0`) or edits (`r>0`) frames with gaps (`z=`, `0` ignored,
//!   negative = gapless skip, default `40ms`); `a=a` selects the current
//!   frame (`c=`), sets gaps, and runs/stops (`s=1` stop, `s=2`
//!   run-loading, `s=3` run, `v=` loops); `a=c` composes rectangles
//!   between frames (a pixel op resolved downstream with the decode).
//! - Layout: placements scale into `c=`/`r=` cell spans (one side alone
//!   preserves aspect downstream), honor source rects and `X=`/`Y=`
//!   offsets downstream, stack by `z=` (negative under text, below
//!   `INT32_MIN/2` under non-default backgrounds; ties break by lower
//!   image id), and move the cursor past the span unless `C=1`.
//! - Lifetime: placements scroll with their grid rows (only when entirely
//!   inside the scrolled region; pushed out means clipped away), die on
//!   full reset, on `ED` full-clear, and on alt-screen switches per the
//!   specification. Text-erase commands otherwise leave them alone.
//!
//! # Bounds (threats T-01, T-02)
//!
//! - [`KITTY_PLACE_MAX_PLACEMENTS`] placements per store (oldest-first
//!   eviction, deterministic like [`crate::image::ImageStore`]).
//! - [`KITTY_ANIM_MAX_IMAGES`] animation descriptors, each holding at
//!   most [`KITTY_ANIM_MAX_FRAMES`] frame gaps: an unbounded frame flood
//!   fails closed (`TooManyFrames`) instead of growing memory. No pixel
//!   data lives here — only `u32` gaps and counters — so the worst case
//!   is tens of kilobytes regardless of claimed dimensions.
//! - No I/O, no clocks: [`KittyAnimation::advance`] is a pure function of
//!   caller-supplied elapsed milliseconds, so playback is testable without
//!   timers and advancement cost is linear in skipped gapless frames with
//!   a zero-duration guard against spinning.
//!
//! Pixel decode, file/shm reads, and paint compositing live downstream
//! (`bitty-graphics` extension, `bitty-render` atlas layers): this store
//! records display intent the renderer can composite once data arrives,
//! and prunes references that can never resolve.

use std::collections::VecDeque;

/// Maximum placements retained per store (bounded per T-01).
///
/// kitty quotas image bytes (320 MiB/buffer); headless placement records
/// are `~64 B` each, so 256 entries cap the store at ~16 KiB while still
/// covering dense `mpv`/`yazi` galleries. Oldest-first eviction keeps the
/// order deterministic for replay.
pub const KITTY_PLACE_MAX_PLACEMENTS: usize = 256;

/// Maximum animation descriptors per store (bounded per T-01).
pub const KITTY_ANIM_MAX_IMAGES: usize = 64;

/// Maximum frame-gap entries per animation (bounded per T-01).
///
/// A new frame past this cap fails closed instead of growing the gap
/// table: no client can pin unbounded memory by streaming frames.
pub const KITTY_ANIM_MAX_FRAMES: usize = 256;

/// Default frame gap in milliseconds (kitty `DEFAULT_GAP`, ghostty
/// `default_gap_ms`): assigned when a frame arrives with `z` omitted or
/// `z=0`. The root frame defaults to gapless (`0`) until `a=a,r=1,z=N`
/// gives it one.
pub const KITTY_ANIM_DEFAULT_GAP_MS: u32 = 40;

/// `z-index` below which placements draw under non-default cell
/// backgrounds (`INT32_MIN/2`, kitty layering rule).
pub const KITTY_Z_BELOW_BACKGROUND: i32 = -1_073_741_824;

/// One image placement: a display of an image on (or under) the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyPlacement {
    /// Wire `i=` image id (`0` = anonymous image).
    pub image_id: u32,
    /// Wire `p=` placement id (`0` = anonymous placement).
    pub placement_id: u32,
    /// Anchor row on the grid (meaningless for virtual prototypes).
    pub anchor_row: u32,
    /// Anchor column on the grid (meaningless for virtual prototypes).
    pub anchor_col: u32,
    /// Cell rows spanned (`0` = derive from pixels downstream).
    pub rows: u16,
    /// Cell columns spanned (`0` = derive from pixels downstream).
    pub cols: u16,
    /// Wire `z=` stacking order (negative draws under text).
    pub z_index: i32,
    /// `U=1` prototype for `U+10EEEE` runs: no anchor, no cursor motion,
    /// immune to physical (cell/row/column/z) delete selectors.
    pub virtual_proto: bool,
    /// `(parent image id, parent placement id)` for relative placement.
    pub parent: Option<(u32, u32)>,
    /// Wire `H=`/`V=` cell offset from the parent origin.
    pub parent_offset: (i32, i32),
    /// Whether the anchor lives on the alternate screen.
    pub on_alt_screen: bool,
}

impl KittyPlacement {
    /// Placement key: `(image id, placement id)`.
    #[must_use]
    pub const fn key(self) -> (u32, u32) {
        (self.image_id, self.placement_id)
    }

    /// Whether this placement may be replaced in place by a re-put of the
    /// same key: only addressed placements (`p != 0`) on real images
    /// replace; anonymous placements always accumulate (specification:
    /// repeated `a=p` with `p=0` yields multiple placements, and `p` on
    /// image `0` is ignored).
    #[must_use]
    pub const fn is_replaceable(self) -> bool {
        self.image_id != 0 && self.placement_id != 0
    }

    /// Whether `z` draws under text.
    #[must_use]
    pub const fn is_below_text(self) -> bool {
        self.z_index < 0
    }

    /// Whether `z` draws under non-default cell backgrounds.
    #[must_use]
    pub const fn is_below_background(self) -> bool {
        self.z_index < KITTY_Z_BELOW_BACKGROUND
    }

    /// Whether the placement's rows lie entirely inside `[top, bottom]`
    /// (the only placements a scroll moves; kitty clips the rest).
    #[must_use]
    pub const fn is_entirely_within(self, top: u32, bottom: u32) -> bool {
        let span = self.rows as u32;
        self.anchor_row >= top && self.anchor_row.saturating_add(span) <= bottom + 1
    }
}

/// Which placements an `a=d` command deletes (wire `d=` selector).
///
/// Lowercase keeps image data for re-display; uppercase (`free_data`)
/// also drops the animation descriptor and number mapping when the image
/// loses its last placement. Virtual prototypes are immune to the
/// physical selectors (`AtCursor`, `AtCell`, `AtCellZ`, `Column`, `Row`,
/// `ZIndex`); only id/number/range selectors reach them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyDeleteSelector {
    /// `a`/`A`: every visible placement on the active screen.
    AllVisible,
    /// `i`/`I`: image id, optionally one placement id.
    ImageId { id: u32, placement: Option<u32> },
    /// `n`/`N`: newest image with number `I=`, optionally one placement.
    NewestNumber { number: u32, placement: Option<u32> },
    /// `c`/`C`: placements intersecting the cursor cell.
    AtCursor,
    /// `f`/`F`: animation frames of an image (placements untouched).
    FramesOf { id: u32 },
    /// `p`/`P`: placements intersecting cell `(x, y)` (1-based cursor
    /// coordinates on the wire).
    AtCell { x: u32, y: u32 },
    /// `q`/`Q`: placements intersecting cell `(x, y)` with z-index `z`.
    AtCellZ { x: u32, y: u32, z: i32 },
    /// `r`/`R`: images with `x <= id <= y`.
    IdRange { lo: u32, hi: u32 },
    /// `x`/`X`: placements intersecting column `x`.
    Column { x: u32 },
    /// `y`/`Y`: placements intersecting row `y`.
    Row { y: u32 },
    /// `z`/`Z`: placements with exactly this z-index.
    ZIndex { z: i32 },
}

impl KittyDeleteSelector {
    /// Whether the selector addresses physical screen locations (which
    /// virtual prototypes never match).
    #[must_use]
    pub const fn is_physical(self) -> bool {
        matches!(
            self,
            Self::AtCursor
                | Self::AtCell { .. }
                | Self::AtCellZ { .. }
                | Self::Column { .. }
                | Self::Row { .. }
                | Self::ZIndex { .. }
        )
    }
}

/// Animation playback state for one image (ghostty `Animation.State`,
/// kitty `a=a` `s=` values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAnimState {
    /// Frames change only via `a=a,c=N` (initial state).
    #[default]
    Stopped,
    /// Advancing; parks on the last frame waiting for more (`s=2`).
    Loading,
    /// Advancing and looping (`s=3`).
    Running,
}

/// Bounded animation descriptor for one image: frame gaps plus playback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyAnimation {
    /// Per-frame gaps in milliseconds, index `0` = root frame gap.
    /// A `0` gap means gapless (skipped during playback).
    gaps: Vec<u32>,
    /// Current frame, 1-based (`1` = root).
    current: u32,
    /// Playback state.
    state: KittyAnimState,
    /// Maximum loops to play (`0` = infinite). Assigned as `v - 1` per
    /// the specification's off-by-one encoding (`v=1` infinite).
    max_loops: u32,
    /// Completed loops since playback started (reset on stop).
    loops_done: u32,
}

/// Why a frame append failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyFrameError {
    /// The animation already holds [`KITTY_ANIM_MAX_FRAMES`] frames.
    TooManyFrames,
    /// Frame number `0` or past the end for edit/gap/current selection.
    NoSuchFrame,
}

impl KittyAnimation {
    /// A fresh descriptor: root frame only, gapless, stopped.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gaps: vec![0],
            current: 1,
            state: KittyAnimState::Stopped,
            max_loops: 0,
            loops_done: 0,
        }
    }

    /// Frame count including the root frame.
    #[must_use]
    pub fn frame_count(&self) -> u32 {
        self.gaps.len() as u32
    }

    /// Current 1-based frame number.
    #[must_use]
    pub const fn current(&self) -> u32 {
        self.current
    }

    /// Playback state.
    #[must_use]
    pub const fn state(&self) -> KittyAnimState {
        self.state
    }

    /// Gap of frame `n` (1-based), if it exists.
    #[must_use]
    pub fn gap(&self, n: u32) -> Option<u32> {
        n.checked_sub(1)
            .and_then(|i| self.gaps.get(i as usize))
            .copied()
    }

    /// Total duration in milliseconds. An all-gapless animation has
    /// duration `0` and never advances: the same guard kitty's
    /// `animation_duration` provides against spinning the skip loop.
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        self.gaps.iter().map(|&g| u64::from(g)).sum()
    }

    /// Appends a frame with `gap_ms` (`0`/absent upstream means
    /// [`KITTY_ANIM_DEFAULT_GAP_MS`]; the caller resolves the default).
    /// Fails closed past [`KITTY_ANIM_MAX_FRAMES`].
    pub fn push_frame(&mut self, gap_ms: u32) -> Result<u32, KittyFrameError> {
        if self.gaps.len() >= KITTY_ANIM_MAX_FRAMES {
            return Err(KittyFrameError::TooManyFrames);
        }
        self.gaps.push(gap_ms);
        Ok(self.gaps.len() as u32)
    }

    /// Sets the gap of frame `n` (1-based). `0` is stored as gapless;
    /// callers that must honor "z=0 ignored" skip the call instead.
    pub fn set_gap(&mut self, n: u32, gap_ms: u32) -> Result<(), KittyFrameError> {
        let slot = n
            .checked_sub(1)
            .and_then(|i| self.gaps.get_mut(i as usize))
            .ok_or(KittyFrameError::NoSuchFrame)?;
        *slot = gap_ms;
        Ok(())
    }

    /// Makes frame `n` current (client-driven animation).
    pub fn set_current(&mut self, n: u32) -> Result<(), KittyFrameError> {
        if n == 0 || n > self.frame_count() {
            return Err(KittyFrameError::NoSuchFrame);
        }
        self.current = n;
        Ok(())
    }

    /// Sets playback state. Entering `Stopped` resets the loop counter
    /// (kitty semantics); leaving it restarts the gap timer, which is
    /// the caller's to re-stamp.
    pub fn set_state(&mut self, state: KittyAnimState) {
        self.state = state;
        if state == KittyAnimState::Stopped {
            self.loops_done = 0;
        }
    }

    /// Sets the loop budget from the wire `v=` value: `0` ignored
    /// (keeps the current budget), `1` infinite, `n > 1` plays `n - 1`
    /// loops. Returns `false` when `v == 0` (ignored).
    pub fn set_loops(&mut self, v: u32) -> bool {
        if v == 0 {
            return false;
        }
        self.max_loops = v.saturating_sub(1);
        true
    }

    /// Advances playback by `elapsed_ms`, returning the new current frame
    /// (1-based) when it changed. Pure function of the argument: no
    /// clocks, no I/O. Stopped animations, single-frame animations, and
    /// zero-duration (all-gapless) animations never advance. `Loading`
    /// parks on the last frame; `Running` wraps, counting loops against
    /// the budget (infinite when `max_loops == 0`). Gapless frames are
    /// skipped without consuming time; the zero-duration guard above is
    /// what keeps that skip from spinning forever.
    pub fn advance(&mut self, elapsed_ms: u64) -> Option<u32> {
        if self.state == KittyAnimState::Stopped || self.gaps.len() < 2 {
            return None;
        }
        if self.duration_ms() == 0 {
            return None;
        }
        let total = self.gaps.len() as u32;
        let mut remaining = elapsed_ms;
        let mut index = self.current - 1;
        loop {
            let gap = u64::from(self.gaps[index as usize]);
            if gap == 0 {
                index = Self::step(index, total);
                if index == 0 && self.on_wrap() {
                    break;
                }
                continue;
            }
            if remaining < gap {
                break;
            }
            remaining -= gap;
            let next = Self::step(index, total);
            if next == 0 {
                if self.state == KittyAnimState::Loading {
                    // Loading parks on the last frame instead of
                    // wrapping: the frame's gap was consumed above.
                    break;
                }
                index = 0;
                if self.on_wrap() {
                    break;
                }
                continue;
            }
            index = next;
        }
        let next = index + 1;
        if next == self.current {
            // Net-zero (exact multiples of the cycle, or only gapless
            // skips that changed nothing visible): no repaint needed.
            None
        } else {
            self.current = next;
            Some(next)
        }
    }

    /// One step forward with wrap (pure helper; callers hold `total >= 2`).
    fn step(index: u32, total: u32) -> u32 {
        if index + 1 >= total { 0 } else { index + 1 }
    }

    /// Records one wrap past the last frame. Returns `true` when the loop
    /// budget is exhausted and playback stopped on the root frame.
    fn on_wrap(&mut self) -> bool {
        self.loops_done = self.loops_done.saturating_add(1);
        if self.max_loops > 0 && self.loops_done >= self.max_loops {
            self.state = KittyAnimState::Stopped;
            self.loops_done = 0;
            true
        } else {
            false
        }
    }
}

impl Default for KittyAnimation {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded placement + animation store for one terminal.
#[derive(Debug, Clone, Default)]
pub struct PlacementStore {
    placements: VecDeque<KittyPlacement>,
    animations: Vec<(u32, KittyAnimation)>,
    /// `(number, image id)` for `I=`-addressed commands; newest wins.
    numbers: Vec<(u32, u32)>,
}

impl PlacementStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of placements retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// Whether no placement is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Number of animation descriptors retained.
    #[must_use]
    pub fn animation_len(&self) -> usize {
        self.animations.len()
    }

    /// Iterates placements oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &KittyPlacement> {
        self.placements.iter()
    }

    /// Placements in paint order: `z-index` ascending, ties by lower
    /// image id, then lower placement id (kitty rule; same id is
    /// insertion-ordered via a stable sort).
    pub fn in_paint_order(&self) -> Vec<&KittyPlacement> {
        let mut order: Vec<&KittyPlacement> = self.placements.iter().collect();
        order.sort_by(|a, b| {
            (a.z_index, a.image_id, a.placement_id).cmp(&(b.z_index, b.image_id, b.placement_id))
        });
        order
    }

    /// Inserts a placement. An addressed re-put (`p != 0` on a real
    /// image) replaces the matching entry; anything else appends, evicting
    /// the oldest entry past [`KITTY_PLACE_MAX_PLACEMENTS`]. A `p != 0`
    /// on image `0` is normalized to anonymous (specification: ignored).
    /// Returns `true` when an entry was replaced.
    pub fn upsert(&mut self, mut placement: KittyPlacement) -> bool {
        if placement.image_id == 0 {
            placement.placement_id = 0;
        }
        if placement.is_replaceable() {
            if let Some(slot) = self
                .placements
                .iter_mut()
                .find(|entry| entry.key() == placement.key())
            {
                *slot = placement;
                return true;
            }
        }
        if self.placements.len() >= KITTY_PLACE_MAX_PLACEMENTS {
            self.placements.pop_front();
        }
        self.placements.push_back(placement);
        false
    }

    /// Looks up one placement by key.
    #[must_use]
    pub fn get(&self, image_id: u32, placement_id: u32) -> Option<&KittyPlacement> {
        self.placements
            .iter()
            .find(|entry| entry.image_id == image_id && entry.placement_id == placement_id)
    }

    /// Removes one placement by key, cascading to relative children.
    /// Returns `true` when something was removed.
    pub fn remove(&mut self, image_id: u32, placement_id: u32) -> bool {
        let before = self.placements.len();
        self.placements
            .retain(|entry| !(entry.image_id == image_id && entry.placement_id == placement_id));
        let removed = self.placements.len() != before;
        if removed {
            self.cascade_orphans();
        }
        removed
    }

    /// Drops relative placements whose parent no longer exists.
    fn cascade_orphans(&mut self) {
        loop {
            let mut orphan: Option<(u32, u32)> = None;
            for entry in &self.placements {
                // Nested `if` (not a let-chain): MSRV 1.85 predates
                // let-chains (stabilized 1.88); `clippy.toml` pins
                // `msrv = "1.85"` so this form is lint-clean.
                if let Some(parent) = entry.parent {
                    if self.get(parent.0, parent.1).is_none() {
                        orphan = Some(entry.key());
                        break;
                    }
                }
            }
            match orphan {
                Some((image, placement)) => self
                    .placements
                    .retain(|entry| !(entry.image_id == image && entry.placement_id == placement)),
                None => break,
            }
        }
    }

    /// Finds a placement to serve as a relative parent: exact
    /// `(image, placement)` match, falling back to any virtual prototype
    /// of the image when the placement is unspecified (`0`).
    #[must_use]
    pub fn resolve_parent(&self, image_id: u32, placement_id: u32) -> Option<&KittyPlacement> {
        if let Some(exact) = self.get(image_id, placement_id) {
            return Some(exact);
        }
        if placement_id == 0 {
            self.placements
                .iter()
                .find(|entry| entry.image_id == image_id && entry.virtual_proto)
        } else {
            None
        }
    }

    /// Applies an `a=d` selector. `cursor` is the 0-based cursor cell for
    /// `AtCursor`; `on_alt` scopes `AllVisible` to the active screen.
    /// `free_data` (uppercase selector) additionally drops animation
    /// descriptors and number mappings left without placements.
    /// Returns the number of placements removed.
    pub fn apply_delete(
        &mut self,
        selector: KittyDeleteSelector,
        cursor: (u32, u32),
        on_alt: bool,
        free_data: bool,
    ) -> usize {
        let mut kill_animation_of: Option<u32> = None;
        let before = self.placements.len();
        match selector {
            KittyDeleteSelector::AllVisible => {
                self.placements
                    .retain(|entry| entry.virtual_proto || entry.on_alt_screen != on_alt);
            }
            KittyDeleteSelector::ImageId { id, placement } => {
                self.placements.retain(|entry| {
                    if entry.image_id != id {
                        return true;
                    }
                    if entry.virtual_proto {
                        // Virtual prototypes die only to id selectors
                        // without a placement pin (any prototype of the
                        // image serves `U+10EEEE` runs).
                        return placement.is_some();
                    }
                    match placement {
                        Some(p) => entry.placement_id != p,
                        None => false,
                    }
                });
                if free_data {
                    kill_animation_of = Some(id);
                }
            }
            KittyDeleteSelector::NewestNumber { number, placement } => {
                if let Some(id) = self.newest_with_number(number) {
                    self.placements.retain(|entry| {
                        if entry.image_id != id {
                            return true;
                        }
                        if entry.virtual_proto {
                            return placement.is_some();
                        }
                        match placement {
                            Some(p) => entry.placement_id != p,
                            None => false,
                        }
                    });
                    if free_data {
                        kill_animation_of = Some(id);
                    }
                }
            }
            KittyDeleteSelector::AtCursor => {
                self.placements.retain(|entry| {
                    entry.virtual_proto
                        || entry.on_alt_screen != on_alt
                        || !covers(entry, cursor.0, cursor.1)
                });
            }
            KittyDeleteSelector::FramesOf { id } => {
                self.animations.retain(|(image, _)| *image != id);
            }
            KittyDeleteSelector::AtCell { x, y } => {
                let (row, col) = (y.saturating_sub(1), x.saturating_sub(1));
                self.placements.retain(|entry| {
                    entry.virtual_proto || entry.on_alt_screen != on_alt || !covers(entry, row, col)
                });
            }
            KittyDeleteSelector::AtCellZ { x, y, z } => {
                let (row, col) = (y.saturating_sub(1), x.saturating_sub(1));
                self.placements.retain(|entry| {
                    entry.virtual_proto
                        || entry.on_alt_screen != on_alt
                        || entry.z_index != z
                        || !covers(entry, row, col)
                });
            }
            KittyDeleteSelector::IdRange { lo, hi } => {
                self.placements
                    .retain(|entry| entry.image_id < lo || entry.image_id > hi);
            }
            KittyDeleteSelector::Column { x } => {
                let col = x.saturating_sub(1);
                self.placements.retain(|entry| {
                    entry.virtual_proto
                        || entry.on_alt_screen != on_alt
                        || col < entry.anchor_col
                        || col >= entry.anchor_col + u32::from(entry.cols.max(1))
                });
            }
            KittyDeleteSelector::Row { y } => {
                let row = y.saturating_sub(1);
                self.placements.retain(|entry| {
                    entry.virtual_proto
                        || entry.on_alt_screen != on_alt
                        || row < entry.anchor_row
                        || row >= entry.anchor_row + u32::from(entry.rows.max(1))
                });
            }
            KittyDeleteSelector::ZIndex { z } => {
                self.placements.retain(|entry| {
                    entry.virtual_proto || entry.on_alt_screen != on_alt || entry.z_index != z
                });
            }
        }
        self.cascade_orphans();
        if free_data {
            self.prune_data_without_placements(kill_animation_of);
        }
        before - self.placements.len()
    }

    /// Drops animation descriptors and number mappings for images that no
    /// longer have placements (`None` = sweep every unreferenced image).
    fn prune_data_without_placements(&mut self, only: Option<u32>) {
        let sweep = |id: u32| -> bool { !self.placements.iter().any(|entry| entry.image_id == id) };
        match only {
            Some(id) => {
                if sweep(id) {
                    self.animations.retain(|(image, _)| *image != id);
                    self.numbers.retain(|(_, mapped)| *mapped != id);
                }
            }
            None => {
                self.animations.retain(|(image, _)| !sweep(*image));
                self.numbers.retain(|(_, mapped)| {
                    self.placements
                        .iter()
                        .any(|entry| entry.image_id == *mapped)
                });
            }
        }
    }

    /// Shifts placements with a scroll-up of `n` rows over
    /// `[top, bottom]`: only placements entirely inside move; any shift
    /// that would leave the region clips the placement away (kitty rule).
    /// Virtual prototypes have no anchor and never move. Only the active
    /// screen's placements move: the hidden screen's anchors stay
    /// untouched underneath (main-screen images survive alt-screen
    /// scrolls and vice versa).
    pub fn scroll_up(&mut self, top: u32, bottom: u32, n: u32, on_alt: bool) {
        self.placements.retain_mut(|entry| {
            if entry.virtual_proto
                || entry.on_alt_screen != on_alt
                || !entry.is_entirely_within(top, bottom)
            {
                return true;
            }
            match entry.anchor_row.checked_sub(n) {
                Some(row) if row >= top => {
                    entry.anchor_row = row;
                    true
                }
                _ => false,
            }
        });
        self.cascade_orphans();
    }

    /// Mirrors [`Self::scroll_up`] downward: rows entering at the top push
    /// anchors down; placements shifted past `bottom` are clipped away.
    /// Screen-gated like [`Self::scroll_up`].
    pub fn scroll_down(&mut self, top: u32, bottom: u32, n: u32, on_alt: bool) {
        self.placements.retain_mut(|entry| {
            if entry.virtual_proto
                || entry.on_alt_screen != on_alt
                || !entry.is_entirely_within(top, bottom)
            {
                return true;
            }
            let row = entry.anchor_row.saturating_add(n);
            let end = row.saturating_add(u32::from(entry.rows));
            if row <= bottom && end <= bottom + 1 {
                entry.anchor_row = row;
                true
            } else {
                false
            }
        });
        self.cascade_orphans();
    }

    /// Clears placements on one screen (`ED` full-clear, alt switches).
    pub fn clear_screen(&mut self, on_alt: bool) {
        self.placements
            .retain(|entry| entry.on_alt_screen != on_alt);
        self.cascade_orphans();
    }

    /// Clears every placement, animation, and number mapping (full reset).
    pub fn clear_all(&mut self) {
        self.placements.clear();
        self.animations.clear();
        self.numbers.clear();
    }

    /// Gets the animation descriptor for an image, if present.
    #[must_use]
    pub fn animation(&self, image_id: u32) -> Option<&KittyAnimation> {
        self.animations
            .iter()
            .find(|(id, _)| *id == image_id)
            .map(|(_, anim)| anim)
    }

    /// Gets or creates the animation descriptor for an image, bounded by
    /// [`KITTY_ANIM_MAX_IMAGES`] (oldest-first eviction). Frame data and
    /// controls for an image nobody transmitted yet still land somewhere
    /// bounded instead of vanishing or growing without limit; the decoder
    /// prunes descriptors that never gain pixels.
    pub fn animation_or_insert(&mut self, image_id: u32) -> &mut KittyAnimation {
        if let Some(index) = self.animations.iter().position(|(id, _)| *id == image_id) {
            return &mut self.animations[index].1;
        }
        if self.animations.len() >= KITTY_ANIM_MAX_IMAGES {
            self.animations.remove(0);
        }
        self.animations.push((image_id, KittyAnimation::new()));
        &mut self.animations.last_mut().expect("just pushed").1
    }

    /// Registers an `I=` number mapping for an image id (bounded like
    /// animations; newest mapping wins for deletes and controls).
    pub fn register_number(&mut self, number: u32, image_id: u32) {
        if number == 0 {
            return;
        }
        self.numbers
            .retain(|(n, id)| *n != number || *id != image_id);
        self.numbers.push((number, image_id));
        while self.numbers.len() > KITTY_ANIM_MAX_IMAGES {
            self.numbers.remove(0);
        }
    }

    /// Newest image id registered under `number`, if any.
    #[must_use]
    pub fn newest_with_number(&self, number: u32) -> Option<u32> {
        self.numbers
            .iter()
            .rev()
            .find(|(n, _)| *n == number)
            .map(|(_, id)| *id)
    }
}

/// Whether the cell `(row, col)` lies inside a placement's span.
/// Unsized placements (`rows`/`cols` `0`, pixels unknown headless) cover
/// exactly their anchor cell.
fn covers(entry: &KittyPlacement, row: u32, col: u32) -> bool {
    let rows = u32::from(entry.rows.max(1));
    let cols = u32::from(entry.cols.max(1));
    row >= entry.anchor_row
        && row < entry.anchor_row.saturating_add(rows)
        && col >= entry.anchor_col
        && col < entry.anchor_col.saturating_add(cols)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(image: u32, placement: u32, row: u32, col: u32) -> KittyPlacement {
        KittyPlacement {
            image_id: image,
            placement_id: placement,
            anchor_row: row,
            anchor_col: col,
            rows: 2,
            cols: 3,
            z_index: 0,
            virtual_proto: false,
            parent: None,
            parent_offset: (0, 0),
            on_alt_screen: false,
        }
    }

    #[test]
    fn upsert_replaces_addressed_reput() {
        let mut store = PlacementStore::new();
        assert!(!store.upsert(placed(5, 3, 0, 0)));
        assert_eq!(store.len(), 1);
        // Same key replaces in place: no flicker, no growth.
        assert!(store.upsert(placed(5, 3, 4, 4)));
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(5, 3).unwrap().anchor_row, 4);
    }

    #[test]
    fn anonymous_placements_accumulate() {
        let mut store = PlacementStore::new();
        // `p=0` with a real image id: multiple placements (spec rule).
        store.upsert(placed(5, 0, 0, 0));
        store.upsert(placed(5, 0, 1, 1));
        assert_eq!(store.len(), 2);
        // Image `0` normalizes `p` away and accumulates too.
        store.upsert(placed(0, 9, 2, 2));
        assert_eq!(store.get(0, 9), None);
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn store_evicts_oldest_past_cap() {
        let mut store = PlacementStore::new();
        for i in 0..KITTY_PLACE_MAX_PLACEMENTS + 10 {
            store.upsert(placed(1, i as u32 + 1, 0, 0));
        }
        assert_eq!(store.len(), KITTY_PLACE_MAX_PLACEMENTS);
        assert!(store.get(1, 1).is_none());
        assert!(store.get(1, 11).is_some());
    }

    #[test]
    fn paint_order_sorts_by_z_then_id() {
        let mut store = PlacementStore::new();
        let mut low = placed(2, 1, 0, 0);
        low.z_index = -1;
        let mut high = placed(1, 1, 0, 0);
        high.z_index = 5;
        let mid = placed(1, 2, 0, 0);
        store.upsert(mid);
        store.upsert(high);
        store.upsert(low);
        let order: Vec<(i32, u32, u32)> = store
            .in_paint_order()
            .iter()
            .map(|p| (p.z_index, p.image_id, p.placement_id))
            .collect();
        assert_eq!(order, [(-1, 2, 1), (0, 1, 2), (5, 1, 1)]);
    }

    #[test]
    fn scroll_moves_only_contained_placements() {
        let mut store = PlacementStore::new();
        store.upsert(placed(1, 1, 4, 0)); // rows 4..6, region 2..8
        let mut straddler = placed(2, 2, 7, 0);
        straddler.rows = 4; // rows 7..11: not entirely inside
        store.upsert(straddler);
        let mut virt = placed(3, 0, 0, 0);
        virt.virtual_proto = true;
        store.upsert(virt);
        store.scroll_up(2, 8, 2, false);
        assert_eq!(store.get(1, 1).unwrap().anchor_row, 2);
        // Straddler untouched; virtual prototype anchorless.
        assert_eq!(store.get(2, 2).unwrap().anchor_row, 7);
        // Scrolling the contained placement out clips it away.
        store.scroll_up(2, 8, 3, false);
        assert!(store.get(1, 1).is_none());
        assert!(store.get(2, 2).is_some());
    }

    #[test]
    fn scroll_down_clips_past_bottom() {
        let mut store = PlacementStore::new();
        store.upsert(placed(1, 1, 6, 0)); // rows 6..8, region 2..8
        store.scroll_down(2, 8, 2, false); // rows 8..10: past bottom
        assert!(store.get(1, 1).is_none());
        store.upsert(placed(2, 2, 2, 0));
        store.scroll_down(2, 8, 2, false);
        assert_eq!(store.get(2, 2).unwrap().anchor_row, 4);
    }

    #[test]
    fn scroll_spares_the_hidden_screen() {
        let mut store = PlacementStore::new();
        store.upsert(placed(1, 1, 4, 0)); // main screen, rows 4..6
        let mut alt = placed(2, 2, 4, 0);
        alt.on_alt_screen = true;
        store.upsert(alt);
        // Alt-screen scroll moves only the alt placement.
        store.scroll_up(2, 8, 2, true);
        assert_eq!(store.get(1, 1).unwrap().anchor_row, 4);
        assert_eq!(store.get(2, 2).unwrap().anchor_row, 2);
        // Main-screen scroll moves only the main placement.
        store.scroll_down(2, 8, 2, false);
        assert_eq!(store.get(1, 1).unwrap().anchor_row, 6);
        assert_eq!(store.get(2, 2).unwrap().anchor_row, 2);
    }

    #[test]
    fn parent_cascade_on_delete() {
        let mut store = PlacementStore::new();
        store.upsert(placed(9, 1, 0, 0));
        let mut child = placed(9, 2, 1, 1);
        child.parent = Some((9, 1));
        store.upsert(child);
        assert!(store.remove(9, 1));
        assert!(store.get(9, 2).is_none());
    }

    #[test]
    fn physical_selectors_skip_virtual_prototypes() {
        let mut store = PlacementStore::new();
        let mut virt = placed(3, 0, 1, 1);
        virt.virtual_proto = true;
        store.upsert(virt);
        store.upsert(placed(4, 4, 1, 1));
        // Cursor-cell delete takes the real placement, spares the proto.
        assert_eq!(
            store.apply_delete(KittyDeleteSelector::AtCursor, (1, 1), false, false),
            1
        );
        assert_eq!(store.len(), 1);
        // Id delete without a pin takes the prototype too.
        assert_eq!(
            store.apply_delete(
                KittyDeleteSelector::ImageId {
                    id: 3,
                    placement: None
                },
                (0, 0),
                false,
                false
            ),
            1
        );
        assert!(store.is_empty());
    }

    #[test]
    fn all_visible_scopes_to_active_screen() {
        let mut store = PlacementStore::new();
        store.upsert(placed(1, 1, 0, 0));
        let mut alt = placed(2, 2, 0, 0);
        alt.on_alt_screen = true;
        store.upsert(alt);
        assert_eq!(
            store.apply_delete(KittyDeleteSelector::AllVisible, (0, 0), false, false),
            1
        );
        assert_eq!(store.len(), 1);
        assert!(store.get(2, 2).is_some());
    }

    #[test]
    fn frame_table_bounded_fail_closed() {
        let mut anim = KittyAnimation::new();
        assert_eq!(anim.frame_count(), 1);
        for _ in 1..KITTY_ANIM_MAX_FRAMES {
            anim.push_frame(KITTY_ANIM_DEFAULT_GAP_MS).unwrap();
        }
        assert_eq!(anim.frame_count(), KITTY_ANIM_MAX_FRAMES as u32);
        assert_eq!(anim.push_frame(40), Err(KittyFrameError::TooManyFrames));
        assert_eq!(anim.set_current(0), Err(KittyFrameError::NoSuchFrame));
        assert_eq!(
            anim.set_current(KITTY_ANIM_MAX_FRAMES as u32 + 1),
            Err(KittyFrameError::NoSuchFrame)
        );
    }

    #[test]
    fn animation_advance_skips_gapless_and_loops() {
        // Root gap 50, frame 2 at 100ms, frame 3 gapless.
        let mut anim = KittyAnimation::new();
        anim.set_gap(1, 50).unwrap();
        anim.push_frame(100).unwrap();
        anim.push_frame(0).unwrap();
        anim.set_state(KittyAnimState::Running);
        // 60ms: root consumed (50), frame 2 shows the rest.
        assert_eq!(anim.advance(60), Some(2));
        // 150ms on frame 2: frame 2 (100), frame 3 skipped, root (50),
        // back on frame 2 with nothing left: net-zero, no repaint.
        assert_eq!(anim.advance(150), None);
        assert_eq!(anim.current(), 2);
        // Long elapsed spans several loops: 960ms = 6 x 150 + 60 lands
        // mid-cycle on frame 2.
        anim.set_current(1).unwrap();
        assert_eq!(anim.advance(960), Some(2));
        // One loop then stop (`v=2`): lands on the root, stopped.
        anim.set_loops(2);
        anim.set_state(KittyAnimState::Running);
        anim.set_current(2).unwrap();
        assert_eq!(anim.advance(260), Some(1));
        assert_eq!(anim.state(), KittyAnimState::Stopped);
        assert_eq!(anim.current(), 1);
    }

    #[test]
    fn gapless_root_is_skipped_immediately() {
        // A gapless current frame never displays: even a short tick
        // moves past it.
        let mut anim = KittyAnimation::new();
        anim.set_gap(1, 0).unwrap();
        anim.push_frame(100).unwrap();
        anim.set_state(KittyAnimState::Running);
        assert_eq!(anim.advance(10), Some(2));
    }

    #[test]
    fn zero_duration_animation_never_advances() {
        let mut anim = KittyAnimation::new();
        anim.push_frame(0).unwrap();
        anim.set_state(KittyAnimState::Running);
        assert_eq!(anim.duration_ms(), 0);
        assert_eq!(anim.advance(u64::MAX), None);
        assert_eq!(anim.current(), 1);
    }

    #[test]
    fn loading_parks_on_last_frame() {
        let mut anim = KittyAnimation::new();
        anim.push_frame(10).unwrap();
        anim.set_state(KittyAnimState::Loading);
        anim.set_current(2).unwrap();
        assert_eq!(anim.advance(1000), None);
        assert_eq!(anim.current(), 2);
    }

    #[test]
    fn number_mapping_resolves_newest() {
        let mut store = PlacementStore::new();
        store.register_number(13, 99);
        store.register_number(13, 100);
        assert_eq!(store.newest_with_number(13), Some(100));
        assert_eq!(store.newest_with_number(7), None);
        // Freeing data prunes the mapping for the freed image; the older
        // mapping for the same number survives (it may still name stored
        // data downstream) and resolves newest-first.
        store.upsert(placed(100, 5, 0, 0));
        assert_eq!(
            store.apply_delete(
                KittyDeleteSelector::NewestNumber {
                    number: 13,
                    placement: None
                },
                (0, 0),
                false,
                true
            ),
            1
        );
        assert_eq!(store.newest_with_number(13), Some(99));
    }

    #[test]
    fn z_partition_predicates() {
        let mut below_text = placed(1, 1, 0, 0);
        below_text.z_index = -1;
        assert!(below_text.is_below_text());
        assert!(!below_text.is_below_background());
        let mut below_bg = placed(1, 1, 0, 0);
        below_bg.z_index = KITTY_Z_BELOW_BACKGROUND - 1;
        assert!(below_bg.is_below_background());
        assert!(!placed(1, 1, 0, 0).is_below_text());
    }
}
