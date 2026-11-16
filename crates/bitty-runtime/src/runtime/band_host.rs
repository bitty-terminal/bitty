//! Core-owned host surface for plugin edge bands (CTX-0946, W-104 C1-C3).
//!
//! The bar plugin's stated host gaps (its `README.md`, read-only from Core):
//! band clicks are parsed but never routed (C1), band painting ignores
//! `fg`/`bg`/`bold` (C2), and plugin bands reserve no exclusive zone (C3).
//! This module closes all three on the host side, with no bar plugin
//! changes:
//!
//! - **C1 — Core-owned click routing.** Pointer geometry is authoritative in
//!   Core: a primary press on a band row is consumed as chrome (no focus
//!   move, no selection, no capture report), and the paired
//!   primary release resolves through the same band rectangles the renderer
//!   used into the owning plugin's declared `on_click` command. The plugin
//!   never sees raw pointer coordinates: the host queues a
//!   [`BandClickRequest`] (owner plugin id, command verb, declared args) and
//!   the application layer dispatches it through the normal
//!   `PluginRuntime::dispatch_command` path, where registration and
//!   capability checks fail closed as usual. Clicks on unclaimed spans and
//!   clicks outside every band row dispatch nothing. Overlapping row claims
//!   (two bands on one row) are denied with a
//!   diagnostic — never routed to either claimant.
//! - **C2 — paint tokens and the redraw gate.** `fg`/`bg` resolve through the
//!   minimal host token table ([`resolve_band_token`]); `bold` paints as a
//!   synthetic double-strike. Every band paint is bounded to its granted
//!   one-row band rectangle: text is clipped to the window width, fills and
//!   glyphs are derived from the same flattened spans the hit-test uses, and
//!   a geometry violation (overlap with another band or the
//!   layout container) skips the whole band — never a partial paint — and
//!   counts [`BandHostStats::paint_violations`].
//! - **C3 — exclusive-zone enforcement.** Visible plugin bands shrink the
//!   layout container through the normal reflow path, so no terminal cell, cursor,
//!   selection, or overlay region is ever painted under a band, and PTY
//!   winsizes follow the reduced grid. Content-only band updates change
//!   damage only; showing, hiding, or restacking a band reflows once.
//!
//! CTX-0979: Core draws no workspace display (Hyprland-style); there is no
//! Core bar row to collide with or to reserve.
//!
//! Issue #2411 adds the Waybar-style layout and pointer surface on the same
//! terms, with no new capability, slot, error code, or protocol:
//!
//! - **Group alignment.** A band root `Row` places its direct children by
//!   declared `anchor` (`left`/`center`/`right`) against the presented width
//!   ([`flatten_band_runs_in_width`]). `center` is centered on the full row,
//!   `right` ends at the inset edge, and an over-subscribed row clips in the
//!   documented priority (left, then right, then center) so runs never
//!   overlap, never exceed the row, and never panic. Hit-testing, region
//!   emission, and paint all read those same runs, so gap cells stay
//!   unclaimed.
//! - **Group distribution (bitty#2191).** The same root `Row` may instead
//!   declare `align` (`start`/`center`/`end`/`space-between`), which
//!   distributes its direct children inside the same bound: `center` and
//!   `end` offset the whole group, and `space-between` spreads the leftover
//!   width across the inter-child gaps. `align` and a child `anchor` are
//!   mutually exclusive (refused at parse, so this walk never sees both), and
//!   an over-subscribed group keeps the plain flow and clips at the band edge.
//!   Nothing here computes a second geometry: hit-test, region emission, and
//!   paint read the runs this walk emits.
//! - **Bounded hover.** `on_hover` bindings dispatch on pointer enter/leave
//!   with the stable region id and a `phase`, through a bounded queue that
//!   coalesces to one dispatch per region per drain ([`Runtime::update_band_hover`],
//!   [`Runtime::drain_band_hovers`]). Hover carries no coordinates, grants no
//!   authority, never captures input, and never sets `modal_active`; leaving
//!   the region (or losing focus) queues the paired `leave`. `enter` is fenced
//!   by the presented origin and deferred until its band frame is published
//!   ([`Runtime::revalidate_band_hover`]); `leave` is deliberately unfenced
//!   because cleanup must still arrive after a version bump or removal.
//! - **Overlay anchor.** [`BandOverlayAnchor`] names a live band item, so the
//!   application can paint a plugin tooltip next to the hovered region
//!   without Core ever handing the plugin pointer geometry.
//!
//! Ownership direction follows the accepted W-74 disposition (Core owns
//! mechanism — geometry, routing, confinement; plugins own presentation)
//! and the W-82 spelling discipline: no new capability, no new slot, no new
//! error code. The only new host surface is the drain queue
//! ([`Runtime::drain_band_clicks`], [`Runtime::drain_band_scrolls`],
//! [`Runtime::drain_band_hovers`]) plus headless statistics, consistent
//! with the overlay/capture conventions (CTX-0941: Core holds the switch,
//! the application moves the bytes).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::band_slots::{BAND_EDGES, BandAxis, BandEdge};
use super::{BandContent, ChromeBands, Runtime};
use bitty_lua::UiAlign;
use bitty_lua::host::LuaValue;
use bitty_lua::ui::{
    ClickArg, ClickCommand, UI_BUTTON_ARG, UI_BUTTON_LEFT, UI_BUTTON_MIDDLE, UI_BUTTON_RIGHT,
    UI_HOVER_PHASE_ARG, UI_HOVER_PHASE_ENTER, UI_HOVER_PHASE_LEAVE, UI_REGION_ARG,
    UI_SCROLL_DELTA_ARG, UiAnchor, UiNode, UiRegion, UiSlot,
};
use bitty_platform::{CursorPosition, MouseButton};
use bitty_render::grid::{Rgba8, ThemePalette};

/// Maximum queued band click requests (CTX-0946 C1).
///
/// Press/release pairs are user-paced; the queue holds one frame of clicks
/// the application has not drained yet. Overflow drops the newest request
/// fail-closed and counts [`BandHostStats::queue_drops`].
pub const BAND_CLICK_QUEUE_MAX: usize = 8;

/// Maximum queued band scroll requests (CTX-1190, issue #2008).
///
/// Wheel ticks are user-paced but burst harder than clicks; consecutive
/// ticks for one target coalesce into a single slot (see
/// `Runtime::queue_band_scroll`), so the queue holds one frame of scrolls
/// the application has not drained yet. Overflow drops the newest request
/// fail-closed and counts [`BandHostStats::scroll_queue_drops`].
pub const BAND_SCROLL_QUEUE_MAX: usize = 8;

/// Maximum cold-phase admissions selected per pump pass (CTX-1332, C-D9/C-D10).
///
/// The accepted bound is the literal `16`, derived as
/// `BAND_CLICK_QUEUE_MAX + BAND_SCROLL_QUEUE_MAX` rather than invented. It is a
/// selection bound over the combined click/scroll/keyboard/invalid-retry work:
/// one pass selects at most this many entries and leaves the unselected tail
/// queued (never `mem::take`). It is deliberately not a byte ceiling, a global
/// budget, or a wall-time quantum; those numeric policies remain deferred.
pub const COLD_ADMISSION_SELECTION_MAX: usize = BAND_CLICK_QUEUE_MAX + BAND_SCROLL_QUEUE_MAX;

/// ANSI index resolving the `accent` band token (CTX-0946 C2).
///
/// The terminal's conventional accent: bright blue, OSC-4-aware through
/// [`ThemePalette::active_index`] so a user remap moves the accent with the
/// rest of the palette. The candidate theme-token contract will own the
/// full vocabulary; until it is accepted this table is the whole host
/// surface.
pub const BAND_ACCENT_ANSI_INDEX: u8 = 12;

/// Maximum per-item hit regions one band may expose (issue #2248).
///
/// Regions are user-facing items (status modules, workspace capsules), not
/// spans: adjacent spans with the same declared identity merge into one
/// region first, so the count tracks distinct items. A band that declares
/// more than this fails closed as a whole — no region on it routes a click
/// or scroll — and counts [`BandHostStats::region_overflows`]. The bound is
/// a policy constant: 64 items is above any sane bar and far below the
/// 2048-node scene cap, so a hostile tree cannot mint an unbounded region
/// table. It does not add a security ceiling; it reuses the existing
/// fail-closed admission posture.
pub const BAND_MAX_REGIONS: usize = 64;

/// Divisor of the window width the vertical (`left`/`right`) bands may never
/// cross together (issue #3171).
///
/// #3165 degraded a whole side to zero columns when its scene could not fit
/// above the column content floor, yet the mount still succeeded: the plugin
/// held a band that reserved, painted, and routed nothing. Scene width is not a
/// mount-time fact either — a resize can make any scene unfittable later — so
/// refusing at mount would be a verdict the next reflow overturns. The bound is
/// therefore placed on the *reservation*: the content area keeps at least
/// `1 / BAND_COLUMN_CONTENT_SHARE_DIVISOR` of the window (and never less than
/// [`MIN_CONTENT_COLS`](super::chrome_band::MIN_CONTENT_COLS)'s effective
/// floor, the host's `chrome_min_cols`), and a band wider than its granted
/// share paints clipped to that share exactly as row-band text past the window
/// width does. Written as a divisor so the integer math stays exact and
/// symmetric: `width - width.div_ceil(2) == width / 2`.
pub const BAND_COLUMN_CONTENT_SHARE_DIVISOR: u16 = 2;

/// One flattened run of a band tree: a char-column span sharing style and
/// click binding (CTX-0946 C1/C2).
///
/// Columns count display cells ([`char_cell_width`](bitty_term_state::char_cell_width)),
/// so the hit-test and the paint path agree on which span owns a pointer
/// column. A container's (`Row`/`Column`/`List`) `fg`/`bg`/`bold`/`on_click`/
/// `on_scroll` applies to descendant spans that do not declare their own
/// (nearest declaration wins); a `Text` leaf's own declaration always wins
/// for its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandRun {
    /// First display-cell column of the span (inclusive).
    pub start_col: usize,
    /// One past the last display-cell column of the span (exclusive).
    pub end_col: usize,
    /// First char index of the span in the flattened text (inclusive).
    pub start_char: usize,
    /// One past the last char index of the span (exclusive).
    pub end_char: usize,
    /// Foreground theme token name, if declared.
    pub fg: Option<String>,
    /// Background theme token name, if declared.
    pub bg: Option<String>,
    /// Whether the span paints bold (synthetic double-strike).
    pub bold: bool,
    /// Click binding effective for the span, if any.
    pub on_click: Option<ClickCommand>,
    /// Scroll binding effective for the span, if any (CTX-1190, #2008).
    pub on_scroll: Option<ClickCommand>,
    /// Hover binding effective for the span, if any (issue #2411).
    ///
    /// A hover binding always carries a `region` (the parser refuses an
    /// anonymous one), so the host has a stable transition identity without
    /// ever carrying pointer coordinates.
    pub on_hover: Option<ClickCommand>,
    /// Per-item hit region effective for the span, if declared (issue #2248).
    ///
    /// One identity per span: the click binding's region wins, else the
    /// scroll binding's. Parser validation guarantees both agree when both
    /// are present. `None` keeps the legacy span-only click target.
    pub region: Option<UiRegion>,
}

/// Declarations one node passes down to its descendant spans (CTX-0946).
///
/// A container's `fg`/`bg`/`bold`/bindings apply to descendant spans that do
/// not declare their own (nearest declaration wins). [`Self::of`] snapshots
/// one node's own declaration in that shape, so a host that has to walk a
/// subtree separately — the overlay stacks a `Column`/`List` root's direct
/// children as rows — can still start each walk under the root's context
/// instead of dropping it (CTX-1419, issue #2191).
///
/// `bold` stays an `Option` so an explicit `bold = false` overrides a `true`
/// ancestor exactly like the single-walk band flattening.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BandInherited {
    fg: Option<String>,
    bg: Option<String>,
    bold: Option<bool>,
    on_click: Option<ClickCommand>,
    on_scroll: Option<ClickCommand>,
    on_hover: Option<ClickCommand>,
    region: Option<UiRegion>,
}

impl BandInherited {
    /// The declaration `node` applies to its descendants.
    pub(crate) fn of(node: &UiNode) -> Self {
        let (fg, bg, bold, on_click, on_scroll, on_hover) = match node {
            UiNode::Text {
                fg,
                bg,
                bold,
                on_click,
                on_scroll,
                on_hover,
                ..
            }
            | UiNode::Row {
                fg,
                bg,
                bold,
                on_click,
                on_scroll,
                on_hover,
                ..
            }
            | UiNode::Column {
                fg,
                bg,
                bold,
                on_click,
                on_scroll,
                on_hover,
                ..
            }
            | UiNode::List {
                fg,
                bg,
                bold,
                on_click,
                on_scroll,
                on_hover,
                ..
            } => (
                fg.clone(),
                bg.clone(),
                *bold,
                on_click.clone(),
                on_scroll.clone(),
                on_hover.clone(),
            ),
        };
        let region = inherited_region(on_click.as_ref(), on_scroll.as_ref(), None);
        Self {
            fg,
            bg,
            bold,
            on_click,
            on_scroll,
            on_hover,
            region,
        }
    }

    /// Layers `node`'s own declaration over this context (nearest wins).
    ///
    /// The region identity follows the same rule as the bindings: the node's
    /// own click/scroll region when it declares one, else the ancestor's
    /// (issue #2248). The hover binding carries its own region identity and
    /// does not participate in the click identity (issue #2411): an item may
    /// be hoverable as one module and clickable as a sub-item.
    fn layered(&self, node: &UiNode) -> Self {
        let own = Self::of(node);
        Self {
            fg: own.fg.or_else(|| self.fg.clone()),
            bg: own.bg.or_else(|| self.bg.clone()),
            bold: own.bold.or(self.bold),
            region: own.region.or_else(|| self.region.clone()),
            on_click: own.on_click.or_else(|| self.on_click.clone()),
            on_scroll: own.on_scroll.or_else(|| self.on_scroll.clone()),
            on_hover: own.on_hover.or_else(|| self.on_hover.clone()),
        }
    }
}

/// One span's effective region identity: the click binding's, else the scroll
/// binding's, else the ancestor's (issue #2248).
fn inherited_region(
    on_click: Option<&ClickCommand>,
    on_scroll: Option<&ClickCommand>,
    parent: Option<UiRegion>,
) -> Option<UiRegion> {
    on_click
        .and_then(|click| click.region.clone())
        .or_else(|| on_scroll.and_then(|scroll| scroll.region.clone()))
        .or(parent)
}

/// Flattens a band tree into its full text plus styled runs (CTX-0946).
///
/// `Row` children join horizontally; `Column`/`List` children join the same
/// way (vertical stacking is deferred, matching the paint path). The text is
/// the concatenation of every `Text` leaf in walk order; each leaf yields
/// exactly one run, even when empty. Total and headless.
///
/// This is the **unbounded** walk: every group flows left-to-right from
/// column 0, which is exactly the pre-#2411 layout. A host that presents one
/// row at a known width uses [`flatten_band_runs_in_width`] so declared group
/// anchors resolve. Both walks emit text in the same order; for an unanchored
/// scene whose leaves all fit they produce the same runs (including the
/// zero-width run of an empty `Text`). The one difference is deliberate: the
/// width walk drops the run of a non-empty leaf that does not fit its slot,
/// so a clipped item can never claim a hit region.
#[must_use]
pub fn flatten_band_runs(root: &UiNode) -> (String, Vec<BandRun>) {
    flatten_band_runs_under(root, &BandInherited::default())
}

/// Flattens `root` under an already-established ancestor declaration
/// (CTX-1419, issue #2191).
///
/// The overlay host stacks a `Column`/`List` root's direct children as rows,
/// so it flattens each child separately; passing the root's declaration here
/// keeps "nearest declaration wins" identical on both hosted surfaces.
#[must_use]
pub(crate) fn flatten_band_runs_under(
    root: &UiNode,
    inherited: &BandInherited,
) -> (String, Vec<BandRun>) {
    let mut emit = BandEmit::default();
    emit.node(root, 0, None, inherited, None);
    (emit.text, emit.runs)
}

/// Flattens a one-row band at its presented width (issue #2411).
///
/// The root `Row` resolves its direct children's group anchors against
/// `width` display cells: `left` groups from column 0, `right` groups flush to
/// `width`, `center` groups centered on the full width. Over-subscribed rows
/// degrade deterministically (left first, then right, then center) by clipping
/// at the group slot boundary, so runs never overlap, never exceed `width`,
/// and never panic. A root that is not a `Row` flows left-to-right unchanged.
///
/// bitty#2191: a root `Row` that declares `align` (instead of child anchors)
/// distributes its direct children inside the same bound — `center` offsets
/// the group by `slack / 2`, `end` by all of the slack, and `space-between`
/// spreads it across the inter-child gaps. An over-subscribed group has no
/// slack and keeps the plain flow, clipped at `width`.
///
/// Runs carry absolute display-cell columns, so the paint path and the
/// hit-test read one geometry. Characters clipped out of their group are not
/// emitted at all: their runs are absent, which is what keeps a clipped item
/// from claiming a hit region.
#[must_use]
pub fn flatten_band_runs_in_width(root: &UiNode, width: usize) -> (String, Vec<BandRun>) {
    let mut emit = BandEmit::default();
    emit.node(root, 0, None, &BandInherited::default(), Some(width));
    (emit.text, emit.runs)
}

/// Accumulates one flattened band walk (issue #2411).
#[derive(Debug, Default)]
struct BandEmit {
    text: String,
    runs: Vec<BandRun>,
    /// Retained chars so `start_char`/`end_char` stay O(1) per leaf instead of
    /// rescanning the growing text on every node (hot-path allocation bound).
    chars: usize,
}

impl BandEmit {
    /// Emits `node` starting at display-cell column `origin`.
    ///
    /// `clip` restricts emitted characters to `[start, end)` display cells.
    /// `bounded_width` makes this node the band root `Row`, whose direct
    /// children are placed by declared anchor.
    fn node(
        &mut self,
        node: &UiNode,
        origin: usize,
        clip: Option<(usize, usize)>,
        inherited: &BandInherited,
        bounded_width: Option<usize>,
    ) {
        if let Some((start, end)) = clip {
            if end <= start {
                // Nothing of this subtree fits its slot; emit nothing so a
                // clipped item cannot claim a hit region. The one exception is
                // an empty `Text` leaf, which keeps its zero-width run (the
                // unbounded walk's shape) clamped onto the slot boundary.
                let empty_leaf = matches!(node, UiNode::Text { text, .. } if text.is_empty());
                if !empty_leaf {
                    return;
                }
            }
        }
        let effective = inherited.layered(node);
        match node {
            UiNode::Text { text: leaf, .. } => {
                let start_char = self.chars;
                let mut natural = 0usize;
                let mut emitted = 0usize;
                let mut run_start: Option<usize> = None;
                for ch in leaf.chars() {
                    let width = usize::from(bitty_term_state::char_cell_width(ch));
                    let cell_start = origin.saturating_add(natural);
                    let cell_end = cell_start.saturating_add(width);
                    natural = natural.saturating_add(width);
                    if let Some((clip_start, clip_end)) = clip {
                        if cell_start < clip_start || cell_end > clip_end {
                            // Keep only the fully visible slice of the leaf:
                            // a clipped character is not emitted at all.
                            continue;
                        }
                    }
                    run_start.get_or_insert(cell_start);
                    self.text.push(ch);
                    emitted = emitted.saturating_add(width);
                    self.chars = self.chars.saturating_add(1);
                }
                let start_col = match run_start {
                    Some(start) => start,
                    // A non-empty leaf clipped entirely out of its slot emits
                    // no run: it paints nothing and must not claim a hit
                    // region. An empty leaf still yields its zero-width run
                    // (the pre-#2411 shape), clamped into its slot so even an
                    // over-subscribed row keeps every span inside the width.
                    None if clip.is_some() && !leaf.is_empty() => return,
                    None => clip.map_or(origin, |(start, end)| origin.clamp(start, end)),
                };
                self.runs.push(BandRun {
                    start_col,
                    end_col: start_col.saturating_add(emitted),
                    start_char,
                    end_char: self.chars,
                    fg: effective.fg,
                    bg: effective.bg,
                    bold: effective.bold.unwrap_or(false),
                    on_click: effective.on_click,
                    on_scroll: effective.on_scroll,
                    on_hover: effective.on_hover,
                    region: effective.region,
                });
            }
            UiNode::Row { children, .. }
            | UiNode::Column { children, .. }
            | UiNode::List { children, .. } => {
                if let Some(width) = bounded_width {
                    // Issue #2411: only the band root Row has a row width to
                    // resolve declared anchors against. bitty#2191: a declared
                    // `align` selects the distribution of the root's own
                    // children instead; the parser refuses the field beside a
                    // child `anchor`, so the two mechanisms never reach this
                    // walk together.
                    let slots = match node.align() {
                        Some(align) => align_slots(children, width, align),
                        None => anchored_slots(children, width),
                    };
                    for (child, slot) in children.iter().zip(slots) {
                        self.node(
                            child,
                            slot.origin,
                            Some((slot.clip_start, slot.clip_end)),
                            &effective,
                            None,
                        );
                    }
                    return;
                }
                let mut cursor = origin;
                for child in children {
                    self.node(child, cursor, clip, &effective, None);
                    cursor = cursor.saturating_add(intrinsic_cells(child));
                }
            }
        }
    }
}

/// One anchored group's resolved placement inside the band row (issue #2411).
///
/// `origin` is the group's natural start; `clip_start`/`clip_end` is the
/// display-cell interval it may paint. Over-subscription clips the group, it
/// never moves or overlaps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupSlot {
    origin: usize,
    clip_start: usize,
    clip_end: usize,
}

/// Resolves the declared anchors of a root `Row`'s direct children
/// (issue #2411).
///
/// Placement policy, in order:
///
/// 1. `left` groups (and unanchored children, which default to `left`) claim
///    `[0, left_total)` in declaration order.
/// 2. `right` groups claim the trailing edge with the **last declared child
///    at the edge** and earlier declared children stacked inward (so the
///    declaration order still reads left-to-right on screen), clipped so they
///    never start before the left groups end.
/// 3. `center` groups stack from the desired full-width center
///    (`(width - center_total) / 2`), pushed right only as far as the left
///    groups require and clipped at the right groups' start.
///
/// Total: `O(children)` time and one `Vec` of slots; the caller never sees an
/// out-of-range or inverted interval.
fn anchored_slots(children: &[UiNode], width: usize) -> Vec<GroupSlot> {
    let widths: Vec<usize> = children.iter().map(intrinsic_cells).collect();
    let anchor_at = |index: usize| children[index].anchor().unwrap_or(UiAnchor::Left);
    let total = |anchor: UiAnchor| {
        children
            .iter()
            .enumerate()
            .filter(|(index, _)| anchor_at(*index) == anchor)
            .map(|(index, _)| widths[index])
            .fold(0usize, usize::saturating_add)
    };
    let left_total = total(UiAnchor::Left).min(width);
    let right_total = total(UiAnchor::Right);
    let center_total = total(UiAnchor::Center);

    let mut slots = vec![
        GroupSlot {
            origin: 0,
            clip_start: 0,
            clip_end: 0,
        };
        children.len()
    ];

    // Left groups: declaration order from the inset edge.
    let mut cursor = 0usize;
    for (index, width) in widths.iter().enumerate() {
        if anchor_at(index) != UiAnchor::Left {
            continue;
        }
        let end = cursor.saturating_add(*width).min(left_total);
        slots[index] = GroupSlot {
            origin: cursor,
            clip_start: cursor.min(end),
            clip_end: end,
        };
        cursor = cursor.saturating_add(*width);
    }

    // Right groups: right-to-left so the first declared group sits innermost.
    let right_start = width.saturating_sub(right_total).max(left_total);
    let mut cursor = width;
    for index in (0..children.len()).rev() {
        if anchor_at(index) != UiAnchor::Right {
            continue;
        }
        let natural = cursor.saturating_sub(widths[index]);
        let clip_start = natural.max(right_start).min(cursor);
        slots[index] = GroupSlot {
            origin: natural,
            clip_start,
            clip_end: cursor,
        };
        cursor = natural;
    }

    // Center groups: true centering on the full width, then clipped into the
    // free middle between the left and right groups.
    let mid_lo = left_total;
    let mid_hi = right_start.max(mid_lo);
    let desired = width.saturating_sub(center_total) / 2;
    let latest = mid_hi.saturating_sub(center_total).max(mid_lo);
    let mut cursor = desired.max(mid_lo).min(latest);
    let center_end = cursor.saturating_add(center_total).min(mid_hi);
    for (index, width) in widths.iter().enumerate() {
        if anchor_at(index) != UiAnchor::Center {
            continue;
        }
        let end = cursor.saturating_add(*width).min(center_end);
        slots[index] = GroupSlot {
            origin: cursor.min(end),
            clip_start: cursor.min(end),
            clip_end: end,
        };
        cursor = cursor.saturating_add(*width);
    }

    slots
}

/// Resolves the declared distribution of a band root `Row`'s direct children
/// (bitty#2191).
///
/// The group's content is the sum of the children's intrinsic widths; the
/// slack is the saturating `width - content`, so an over-subscribed group has
/// no slack at all: it degrades to the plain flow and clips at the band edge
/// under the existing walk rather than receiving a negative offset.
///
/// - `start` — origin `0`, today's left-to-right flow, byte for byte.
/// - `center` — origin `slack / 2` (integer division, so the left side takes
///   the smaller half), which centres the whole child group.
/// - `end` — origin `slack`, so the last child ends exactly at the band edge.
/// - `space-between` — origin `0`, and `slack` is split across the
///   `children - 1` inter-child gaps as `slack / gaps` with the remainder
///   handed to the leftmost gaps, so the first child stays flush left and the
///   last flush right.
///
/// A childless or single-child row has no inter-child gap to spread, so
/// `space-between` leaves the child flush left: the distribution is inert
/// there, not an error, and the painted result stays inside the band.
///
/// Total: `O(children)` time and one `Vec` of slots, exactly like
/// [`anchored_slots`]; every returned interval is inside `[0, width]`.
fn align_slots(children: &[UiNode], width: usize, align: UiAlign) -> Vec<GroupSlot> {
    let widths: Vec<usize> = children.iter().map(intrinsic_cells).collect();
    let content = widths
        .iter()
        .fold(0usize, |total, cells| total.saturating_add(*cells));
    let slack = width.saturating_sub(content);
    let gaps = children.len().saturating_sub(1);
    let mut gap_cells = vec![0usize; gaps];
    let origin = match align {
        UiAlign::Start => 0,
        UiAlign::Center => slack / 2,
        UiAlign::End => slack,
        UiAlign::SpaceBetween => {
            // No pair means no gap to spread: the group stays flush left. The
            // `checked_div` is the whole guard, so the remainder is derived
            // from the quotient rather than dividing twice.
            if let Some(share) = slack.checked_div(gaps) {
                let remainder = slack.saturating_sub(share.saturating_mul(gaps));
                for (index, gap) in gap_cells.iter_mut().enumerate() {
                    *gap = share.saturating_add(usize::from(index < remainder));
                }
            }
            0
        }
    };
    // The group's own extent: its origin, its content, and the gaps the
    // distribution inserted, clamped to the row so no slot can escape it.
    let group_end = origin
        .saturating_add(content)
        .saturating_add(
            gap_cells
                .iter()
                .fold(0usize, |total, gap| total.saturating_add(*gap)),
        )
        .min(width);
    let mut slots = Vec::with_capacity(children.len());
    let mut cursor = origin;
    for (index, cells) in widths.iter().enumerate() {
        let end = cursor.saturating_add(*cells).min(group_end);
        slots.push(GroupSlot {
            origin: cursor,
            clip_start: cursor.min(end),
            clip_end: end,
        });
        cursor = cursor.saturating_add(*cells);
        if let Some(gap) = gap_cells.get(index) {
            cursor = cursor.saturating_add(*gap);
        }
    }
    slots
}

/// Display-cell width of one subtree laid out without slack.
fn intrinsic_cells(node: &UiNode) -> usize {
    match node {
        UiNode::Text { text, .. } => display_cells(text),
        UiNode::Row { children, .. }
        | UiNode::Column { children, .. }
        | UiNode::List { children, .. } => children
            .iter()
            .map(intrinsic_cells)
            .fold(0usize, usize::saturating_add),
    }
}

/// Display-cell width of `text` under the terminal cell rules.
fn display_cells(text: &str) -> usize {
    text.chars()
        .map(|ch| usize::from(bitty_term_state::char_cell_width(ch)))
        .sum()
}

/// Whether a band reserves a row (CTX-0946 C3, CTX-0925 item 3).
///
/// An empty-text band is hidden: it takes no stacking row, reserves no
/// exclusive zone, paints nothing, and routes no clicks. Anything else —
/// including whitespace-only text, which still paints fills — is visible.
#[must_use]
pub fn band_is_visible(root: &UiNode) -> bool {
    let (text, _) = flatten_band_runs(root);
    !text.is_empty()
}

/// Display-cell width of a column band's content (issue #3165).
///
/// A column band stacks one content row per direct child of a `Column`/`List`
/// root, so its thickness is the widest such row in display cells. An
/// axis-neutral `Text` root (or any other root, which the mount gate refuses
/// for `left`/`right`) is measured as one row. Saturating, so a hostile tree
/// can never wrap the width; the caller clamps it to the window width.
#[must_use]
pub fn column_band_width(root: &UiNode) -> u16 {
    let widest = match root {
        UiNode::Column { children, .. } | UiNode::List { children, .. } => children
            .iter()
            .map(intrinsic_cells)
            .fold(0usize, usize::max),
        _ => intrinsic_cells(root),
    };
    u16::try_from(widest).unwrap_or(u16::MAX)
}

/// Content rows of a column band, top-down, bounded by `max_rows`
/// (issue #3165).
///
/// One row per direct child of a `Column`/`List` root, each flattened under
/// the root's own declaration (the same row stack the overlay host paints),
/// so a root-level `fg`/`bg`/`bold` reaches its rows and a nested `Row` still
/// joins horizontally. Any other root is its single flattened line. `max_rows`
/// bounds the work to the rows the band can actually paint, so a scene at the
/// `SCN-1` node ceiling never flattens rows past the screen.
#[must_use]
pub fn column_band_rows(root: &UiNode, max_rows: usize) -> Vec<(String, Vec<BandRun>)> {
    let inherited = BandInherited::of(root);
    let mut rows: Vec<(String, Vec<BandRun>)> = Vec::new();
    match root {
        UiNode::Column { children, .. } | UiNode::List { children, .. } => {
            for child in children.iter().take(max_rows) {
                rows.push(flatten_band_runs_under(child, &inherited));
            }
        }
        _ if max_rows > 0 => rows.push(flatten_band_runs(root)),
        _ => {}
    }
    rows
}

/// One bounded per-item hit region emitted by a band (issue #2248).
///
/// The rectangle is one painted line high and spans display cells `[start_col,
/// end_col)` inside the band, already clipped to the band's granted width. Row
/// bands are one line tall, so `row` is the band row; a column band emits one
/// line per root child (issue #3171), so `row` is that content line's window row
/// and `start_col` is still measured from the band's own left edge — the same
/// in-band unit the click/scroll/hover hit-test uses. Core derives it from the
/// same flattened runs the paint path uses, so there is no second geometry
/// source. The plugin never supplies coordinates: it declares an identity
/// beside a binding, and the host owns the rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandItemRegion {
    /// Owning plugin id (the band owner).
    pub plugin_id: String,
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible index on the edge (hidden bands take no index).
    pub visible_index: usize,
    /// Window row the band paints (the content line's row for a column band).
    pub row: u16,
    /// Declared region identity (`id` + `kind`).
    pub region: UiRegion,
    /// First display-cell column of the region inside the band (inclusive,
    /// clipped).
    pub start_col: usize,
    /// One past the last display-cell column inside the band (exclusive,
    /// clipped).
    pub end_col: usize,
    /// Exact band tree version that produced this rect.
    pub version: u32,
}

/// Why region emission refused a band (issue #2248).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandRegionError {
    /// No visible band at that edge/index.
    MissingBand,
    /// The band declared more than [`BAND_MAX_REGIONS`] distinct regions.
    ///
    /// Fail-closed: the whole band's region table is refused, so no click or
    /// scroll on it can route. The frame still paints.
    Overflow {
        /// Distinct regions the band declared.
        count: usize,
        /// The bound ([`BAND_MAX_REGIONS`]).
        max: usize,
    },
}

/// Derives the bounded region table for one row band (issue #2248).
///
/// Walks the flattened runs in declaration order, keeps spans that declare a
/// region, clips them to `window_width` display cells, and merges adjacent
/// spans with the same identity into one rect (a container region covering
/// several leaves is one item, not one item per leaf). Adjacent but
/// *different* regions stay separate. The result is sorted by `start_col` by
/// construction. More than [`BAND_MAX_REGIONS`] distinct regions refuse the
/// whole table.
pub fn band_regions_for(
    band: &BandContent,
    edge: BandEdge,
    visible_index: usize,
    row: u16,
    window_width: usize,
) -> Result<Vec<BandItemRegion>, BandRegionError> {
    let (_, runs) = flatten_band_runs_in_width(&band.root, window_width);
    collect_band_regions(
        band,
        edge,
        visible_index,
        window_width,
        std::iter::once((row, runs)),
    )
}

/// Derives the bounded region table for one column band (issue #3171).
///
/// The column mirror of [`band_regions_for`]: one line per direct child of the
/// root, each clipped to the band's granted `width` cells, starting at
/// `first_row` and bounded by `max_rows` exactly like the paint walk. The same
/// merge and [`BAND_MAX_REGIONS`] accounting apply, so a column band can never
/// grow a second, divergent region rule.
pub fn column_band_regions_for(
    band: &BandContent,
    edge: BandEdge,
    visible_index: usize,
    first_row: u16,
    width: u16,
    max_rows: usize,
) -> Result<Vec<BandItemRegion>, BandRegionError> {
    let lines = column_band_rows(&band.root, max_rows)
        .into_iter()
        .enumerate()
        .filter_map(|(offset, (_, runs))| {
            let row = first_row.checked_add(u16::try_from(offset).ok()?)?;
            Some((row, runs))
        });
    collect_band_regions(band, edge, visible_index, usize::from(width), lines)
}

/// Shared region walk for both band axes (issue #3171).
///
/// `lines` yields `(window row, flattened spans)` in paint order; `clip_width`
/// is the cell width the band was granted (the window width for a row band, the
/// band's own width for a column band). Adjacency merging is per line: two
/// lines never merge into one rect, so a column band's geometry stays one rect
/// per painted line.
fn collect_band_regions<I>(
    band: &BandContent,
    edge: BandEdge,
    visible_index: usize,
    clip_width: usize,
    lines: I,
) -> Result<Vec<BandItemRegion>, BandRegionError>
where
    I: IntoIterator<Item = (u16, Vec<BandRun>)>,
{
    let mut regions: Vec<BandItemRegion> = Vec::new();
    let mut count = 0usize;
    for (row, runs) in lines {
        // Identity and end column of the trailing distinct region, used both
        // for adjacent-merge detection and to keep counting past the cap.
        let mut trailing: Option<(UiRegion, usize)> = None;
        for run in runs {
            let Some(region) = run.region else {
                trailing = None;
                continue;
            };
            let start = run.start_col.min(clip_width);
            let end = run.end_col.min(clip_width);
            if start >= end {
                // Entirely clipped past the painted bounds; it breaks adjacency.
                trailing = None;
                continue;
            }
            if let Some((previous, previous_end)) = &trailing {
                if previous == &region && *previous_end == start {
                    if let Some(back) = regions.last_mut() {
                        if back.region == region {
                            back.end_col = end;
                        }
                    }
                    trailing = Some((region, end));
                    continue;
                }
            }
            count = count.saturating_add(1);
            if regions.len() < BAND_MAX_REGIONS {
                regions.push(BandItemRegion {
                    plugin_id: band.plugin_id.clone(),
                    edge,
                    visible_index,
                    row,
                    region: region.clone(),
                    start_col: start,
                    end_col: end,
                    version: band.version,
                });
            }
            trailing = Some((region, end));
        }
    }
    if count > BAND_MAX_REGIONS {
        return Err(BandRegionError::Overflow {
            count,
            max: BAND_MAX_REGIONS,
        });
    }
    Ok(regions)
}

/// Resolves a display-cell row and column to exactly one emitted region
/// (issue #2248, issue #3171).
///
/// Later registration wins when regions overlap ("topmost/last-registered"):
/// emitted tables from [`band_regions_for`] are disjoint by construction, so
/// the tie-break is a documented invariant for any future overlapping
/// source rather than a silent first-match. A row band's table carries one
/// row, so the row term is constant there; a column band's table carries one
/// row per painted line, so the line decides which item can own the column.
/// A cell outside every region resolves to `None` (unclaimed chrome).
#[must_use]
pub fn resolve_item_region(
    regions: &[BandItemRegion],
    row: u16,
    col: usize,
) -> Option<&BandItemRegion> {
    regions
        .iter()
        .rev()
        .find(|region| region.row == row && col >= region.start_col && col < region.end_col)
}

/// Mouse button a band region routes (issue #2248).
///
/// Bands consume left, right, and middle presses so no button can reach a
/// clipboard paste or capture path behind chrome. Back/forward and other
/// buttons fall through unchanged (no band action is declared for them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandButton {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Middle button.
    Middle,
}

impl BandButton {
    /// Maps a platform button to a band-routable button.
    #[must_use]
    pub const fn from_mouse(button: MouseButton) -> Option<Self> {
        match button {
            MouseButton::Left => Some(Self::Left),
            MouseButton::Right => Some(Self::Right),
            MouseButton::Middle => Some(Self::Middle),
            MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => None,
        }
    }

    /// Dispatch spelling carried under `button`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Left => UI_BUTTON_LEFT,
            Self::Right => UI_BUTTON_RIGHT,
            Self::Middle => UI_BUTTON_MIDDLE,
        }
    }
}

/// Default Waybar-inspired foreground token for a `bitty-terminal.bar` module
/// span that declares neither `fg` nor `bg`, keyed by its `bar:module:<name>`
/// hover region identity.
///
/// Only the identity is consulted: the region id is `bar:module:<name>[:…]`,
/// the first segment after the prefix selects the token, and an unknown or
/// missing region yields `None` (the caller keeps the span default). A run
/// that names its own `fg` or `bg` opts out entirely. This is a Core-owned
/// palette role name, never a raw colour and never plugin-chosen.
#[must_use]
pub fn default_band_run_fg_token(run: &BandRun) -> Option<&'static str> {
    if run.fg.is_some() || run.bg.is_some() {
        return None;
    }
    let raw = run
        .on_hover
        .as_ref()
        .and_then(|cmd| cmd.region.as_ref().map(|r| r.id.as_str()))?;
    let module = raw.strip_prefix("bar:module:")?;
    let base = module.split(':').next().unwrap_or(module);
    match base {
        "clock" => Some("accent"),
        "network" => Some("cyan"),
        "cpu" => Some("green"),
        "memory" => Some("magenta"),
        "battery" => Some("ok"),
        "pulseaudio" => Some("yellow"),
        "microphone" => Some("teal"),
        "bluetooth" => Some("blue"),
        "backlight" => Some("bright_yellow"),
        "idle" => Some("purple"),
        "spectrum" => Some("cyan"),
        _ => None,
    }
}

/// Host theme-token table for band paint (CTX-0946 C2, bitty#3108).
///
/// Resolves semantic role tokens (`foreground`, `background`, `cursor`,
/// `selection`, `accent`, `normal`, `muted`, `dim`, `surface`, `ok`,
/// `success`, `warning`, `warn`, `critical`, `error`) and named ANSI 16
/// palette tokens (`black`, `red`, `green`, `yellow`, `orange`, `blue`,
/// `magenta`, `purple`, `cyan`, `teal`, `white`, plus `bright_*` and
/// `ansi.*` forms) against the active theme palette (OSC-4-aware). Returns
/// `None` for unknown tokens; callers substitute the span default and count
/// [`BandHostStats::unknown_tokens`].
#[must_use]
pub fn resolve_band_token(token: &str, palette: &ThemePalette) -> Option<Rgba8> {
    let ansi = |idx: u8| -> Option<Rgba8> {
        let rgb = palette.active_index(idx);
        Some([rgb[0], rgb[1], rgb[2], 0xFF])
    };
    match token {
        "foreground" | "normal" => Some(palette.foreground),
        "background" => Some(palette.background),
        "cursor" => Some(palette.cursor),
        "selection" | "surface" | "surface.0" | "surface.1" | "surface.2" => {
            Some(palette.selection)
        }
        "accent" | "bright_blue" | "ansi.bright_blue" => ansi(BAND_ACCENT_ANSI_INDEX),
        "black" | "ansi.black" => ansi(0),
        "critical" | "error" | "red" | "ansi.red" => ansi(1),
        "ok" | "success" | "green" | "ansi.green" => ansi(2),
        "warning" | "warn" | "yellow" | "orange" | "ansi.yellow" => ansi(3),
        "blue" | "ansi.blue" => ansi(4),
        "magenta" | "purple" | "ansi.magenta" => ansi(5),
        "cyan" | "teal" | "ansi.cyan" => ansi(6),
        "white" | "ansi.white" => ansi(7),
        "muted" | "dim" | "bright_black" | "ansi.bright_black" => ansi(8),
        "bright_red" | "ansi.bright_red" => ansi(9),
        "bright_green" | "ansi.bright_green" => ansi(10),
        "bright_yellow" | "ansi.bright_yellow" => ansi(11),
        "bright_magenta" | "ansi.bright_magenta" => ansi(13),
        "bright_cyan" | "ansi.bright_cyan" => ansi(14),
        "bright_white" | "ansi.bright_white" => ansi(15),
        _ => None,
    }
}

/// Core-owned plain-data origin captured at an eligible presentation
/// (CTX-1332, C-D4).
///
/// The origin is the fencing identity of one painted band: the exact owning
/// plugin generation, mount handle, accepted slot, tree version, edge, and
/// painted geometry. It is captured from the live mount registry when the
/// band snapshot is built and becomes executable only when the frame that
/// carried it was actually presented. Admission never reconstructs it at
/// drain time.
///
/// Additive Rust-only API in the 0.0.x line: `#[doc(hidden)]` and not part of
/// the Lua/SDK wire contract. A missing origin can never mint an executable
/// action; [`Runtime::band_origin_is_current`] and
/// `PluginRuntime::dispatch_ui_command` both fail closed.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandOrigin {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Plugin activation generation the mount belongs to.
    pub generation: u32,
    /// Generation-owned mount handle from the UI block registry.
    pub handle: i64,
    /// Accepted slot the block was mounted into.
    pub slot: UiSlot,
    /// Exact tree version at capture time.
    pub version: u32,
    /// Edge the band was placed on.
    pub edge: BandEdge,
    /// Visible-band index on that edge (hidden bands take no index).
    pub visible_index: u16,
    /// Painted window row at capture time (the first painted row for a column
    /// band, which spans the row-carved container).
    pub row: u16,
    /// Leftmost window column at capture time; `0` for a row band, which spans
    /// the window width and has no column origin (issue #3171).
    ///
    /// The axis is already carried by [`Self::edge`]
    /// ([`BandEdge::axis`]), so the geometry pair is `(row, column)` under a
    /// known axis: a row band's freshness is its row, a column band's is the
    /// column its granted range starts at. Keeping both on the origin means a
    /// reflow that moves only the columns invalidates a column band's captured
    /// actions instead of silently retargeting them.
    pub column: u16,
}

/// One live mount projected for origin capture (CTX-1332, C-D4).
///
/// `TerminalApp` projects the plugin runtime's mounted blocks into this plain
/// shape; [`Runtime::set_chrome_bands_with_mounts`] joins it with the band
/// placement and geometry. Kept separate from [`BandContent`] so the public
/// band snapshot type and `ChromeBands::from_mounts` are unchanged.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandMount {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Accepted slot the block was mounted into.
    pub slot: UiSlot,
    /// Exact tree version at capture time.
    pub version: u32,
    /// Plugin activation generation the mount belongs to.
    pub generation: u32,
    /// Generation-owned mount handle from the UI block registry.
    pub handle: i64,
}

/// Damage identity of one mounted band for the present gate (CTX-1332, F7).
///
/// Includes the band edge (F7: an edge move at an unchanged version must not
/// compare equal) and, for painted bands, the full [`BandOrigin`] so a
/// replacement mount whose version restarts at 1 still forces a frame and
/// still refuses stale actions.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandFrameIdentity {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Accepted slot the block was mounted into.
    pub slot: UiSlot,
    /// Exact tree version.
    pub version: u32,
    /// Edge the band is placed on.
    pub edge: BandEdge,
    /// Executable origin for painted bands; `None` for hidden bands, which are
    /// never clickable. Both axes publish an origin (issue #3171): a column
    /// band's origin carries its column geometry instead of a row.
    pub origin: Option<BandOrigin>,
}

/// One selected cold-phase admission (CTX-1332, C-D9/C-D10).
///
/// The runtime selects at most [`COLD_ADMISSION_SELECTION_MAX`] entries per
/// pump pass and leaves the unselected tail queued.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq)]
pub enum BandAdmission {
    /// One Core-routed band click.
    Click(BandClickRequest),
    /// One Core-routed band scroll.
    Scroll(BandScrollRequest),
}

/// One Core-routed band click: owner, verb, and declared args (CTX-0946 C1).
///
/// The plugin never sees pointer coordinates — only this declared triple,
/// dispatched by the application through `PluginRuntime::dispatch_command`,
/// whose registration and capability gates fail closed as usual.
#[derive(Debug, Clone, PartialEq)]
pub struct BandClickRequest {
    /// Owning plugin id (the band that was hit).
    pub plugin_id: String,
    /// Command verb, unqualified (`focus`, never `other-plugin:verb`).
    pub command: String,
    /// Declared `on_click.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Declared per-item region, when the hit span declared one (issue #2248).
    ///
    /// Dispatch carries the region id under [`UI_REGION_ARG`]. `None` is the
    /// legacy span-only target; the click still routes, without a region
    /// identity.
    pub region: Option<UiRegion>,
    /// Button that produced this click (issue #2248).
    ///
    /// Dispatch carries its spelling under [`UI_BUTTON_ARG`]. Bands never
    /// consult the button for authority; the plugin decides what each
    /// button does through its registered command.
    pub button: BandButton,
    /// Presentation-captured origin (CTX-1332, C-D4).
    ///
    /// Stamped when the gesture is armed from the last eligible presented
    /// frame. A request without one is legacy/test-only and is refused by the
    /// cold pump, so it can never mint an executable action.
    #[doc(hidden)]
    pub origin: Option<BandOrigin>,
    /// Monotonic admission sequence assigned when the gesture is queued.
    ///
    /// `0` on unqueued/test-only requests. Counter exhaustion fails closed at
    /// admission (never wraps).
    #[doc(hidden)]
    pub admission_seq: u64,
}

/// Extracts the dispatch verb from a declared `on_click.command` (CTX-0946 C1).
///
/// Accepts the bar's qualified form (`owner:verb`, qualifier must equal the
/// band owner) and the short form (`verb`, owner-implied). Anything else —
/// a foreign qualifier (no cross-plugin routing), an empty verb, or a
/// malformed qualifier — returns `None` (denied with a diagnostic).
#[must_use]
pub fn split_band_command(command: &str, owner: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    match command.split_once(':') {
        None => Some(command.to_string()),
        Some((qualifier, verb)) => {
            if qualifier != owner || verb.is_empty() || verb.contains(':') {
                return None;
            }
            Some(verb.to_string())
        }
    }
}

/// Converts declared click args to the single Lua table arg (CTX-0946 C1).
///
/// `dispatch_command` takes positional args; a click carries one named
/// table (`run({id = 3})`), preserving declaration order. Issue #2248 adds
/// the resolved region id under [`UI_REGION_ARG`] and the button spelling
/// under [`UI_BUTTON_ARG`] only when the hit span declared a region; both
/// merge last and overwrite declared entries of the same names — never
/// duplicated and never exceeding `UI_CLICK_MAX_ARGS` (the parser reserves
/// their slots). A legacy region-less span keeps its pre-2248 payload
/// byte-for-byte, so strict command schemas stay valid.
#[must_use]
pub fn band_click_args_table(
    args: &[(String, ClickArg)],
    region: Option<&UiRegion>,
    button: BandButton,
) -> LuaValue {
    let Some(region) = region else {
        return band_click_args_table_legacy(args);
    };
    let mut table: Vec<(LuaValue, LuaValue)> = args
        .iter()
        .filter(|(key, _)| key != UI_BUTTON_ARG && key != UI_REGION_ARG)
        .map(|(key, value)| (LuaValue::String(key.clone()), click_arg_value(value)))
        .collect();
    table.push((
        LuaValue::String(UI_REGION_ARG.to_string()),
        LuaValue::String(region.id.clone()),
    ));
    table.push((
        LuaValue::String(UI_BUTTON_ARG.to_string()),
        LuaValue::String(button.as_str().to_string()),
    ));
    LuaValue::Table(table)
}

/// Pre-2248 click payload: declared args only, no host region/button.
fn band_click_args_table_legacy(args: &[(String, ClickArg)]) -> LuaValue {
    LuaValue::Table(
        args.iter()
            .map(|(key, value)| (LuaValue::String(key.clone()), click_arg_value(value)))
            .collect(),
    )
}

/// Marshals one declared arg value.
fn click_arg_value(value: &ClickArg) -> LuaValue {
    match value {
        ClickArg::String(text) => LuaValue::String(text.clone()),
        ClickArg::Number(number) => LuaValue::Number(*number),
        ClickArg::Bool(flag) => LuaValue::Bool(*flag),
    }
}

/// Outcome of resolving a pointer column inside one band (CTX-0946 C1).
#[derive(Debug, Clone, PartialEq)]
pub enum BandColumnOutcome {
    /// A claimed span: route to the owner's declared command.
    Hit(BandClickRequest),
    /// No span claims the column (separator, suffix, padding): route nothing.
    NoClaim,
    /// The claim is unroutable (foreign qualifier, empty verb): deny loudly.
    Denied,
}

/// Resolves a flattened-text column into its click outcome (CTX-0946 C1).
///
/// Pure over one band: finds the run owning `col` (display-cell column) and
/// splits its command against `owner`. Pure for headless tests; geometry
/// (which band owns the row) lives on [`Runtime::plugin_band_hit`].
#[must_use]
pub fn resolve_click_in_band(
    owner: &str,
    runs: &[BandRun],
    col: usize,
    button: BandButton,
) -> BandColumnOutcome {
    let Some(run) = find_run_index(runs, col).and_then(|index| runs.get(index)) else {
        return BandColumnOutcome::NoClaim;
    };
    let Some(click) = run.on_click.as_ref() else {
        return BandColumnOutcome::NoClaim;
    };
    // Pre-2248 spans have no region and no button contract: only the
    // primary button routes, so an existing strict args schema never sees a
    // new `button` entry. Region-aware spans route all three buttons.
    if run.region.is_none() && button != BandButton::Left {
        return BandColumnOutcome::NoClaim;
    }
    let Some(verb) = split_band_command(&click.command, owner) else {
        return BandColumnOutcome::Denied;
    };
    BandColumnOutcome::Hit(BandClickRequest {
        plugin_id: owner.to_string(),
        command: verb,
        args: click.args.clone(),
        region: run.region.clone(),
        button,
        origin: None,
        admission_seq: 0,
    })
}

/// Index of the run owning a display-cell column, if any (CTX-0946 C1).
///
/// Pure run identity behind [`resolve_click_in_band`]: press and release
/// compare run indices (plus band and resolved request) so a press-drag
/// onto another span can never fire its command. Declared regions pair by
/// identity instead (issue #2248), so a merged multi-leaf region is one
/// item.
#[must_use]
pub fn find_run_index(runs: &[BandRun], col: usize) -> Option<usize> {
    runs.iter()
        .position(|run| col >= run.start_col && col < run.end_col)
}

/// Resolves a click on one declared region (issue #2248).
///
/// Finds the run under the column that carries `region` (the emitted rect
/// is derived from those runs, so the two geometries agree) and splits its
/// click command against `owner`. A region declared only on `on_scroll`
/// yields `NoClaim` for a click; a foreign qualifier yields `Denied`.
fn resolve_click_for_region(
    owner: &str,
    runs: &[BandRun],
    region: &UiRegion,
    col: usize,
    button: BandButton,
) -> BandColumnOutcome {
    let Some(run) = runs.iter().find(|run| {
        col >= run.start_col && col < run.end_col && run.region.as_ref() == Some(region)
    }) else {
        return BandColumnOutcome::NoClaim;
    };
    let Some(click) = run.on_click.as_ref() else {
        return BandColumnOutcome::NoClaim;
    };
    let Some(verb) = split_band_command(&click.command, owner) else {
        return BandColumnOutcome::Denied;
    };
    BandColumnOutcome::Hit(BandClickRequest {
        plugin_id: owner.to_string(),
        command: verb,
        args: click.args.clone(),
        region: Some(region.clone()),
        button,
        origin: None,
        admission_seq: 0,
    })
}

/// Resolves a wheel on one declared region (issue #2248).
fn resolve_scroll_for_region(
    owner: &str,
    runs: &[BandRun],
    region: &UiRegion,
    col: usize,
) -> BandScrollOutcome {
    let Some(run) = runs.iter().find(|run| {
        col >= run.start_col && col < run.end_col && run.region.as_ref() == Some(region)
    }) else {
        return BandScrollOutcome::NoClaim;
    };
    let Some(scroll) = run.on_scroll.as_ref() else {
        return BandScrollOutcome::NoClaim;
    };
    let Some(verb) = split_band_command(&scroll.command, owner) else {
        return BandScrollOutcome::Denied;
    };
    BandScrollOutcome::Hit(BandScrollTarget {
        plugin_id: owner.to_string(),
        command: verb,
        args: scroll.args.clone(),
        region: Some(region.clone()),
        origin: None,
    })
}

/// One Core-routed band scroll: owner, verb, declared args, and the
/// host-computed line step (CTX-1190, issue #2008).
///
/// Like [`BandClickRequest`] the plugin never sees pointer coordinates —
/// only this declared triple plus `delta`, dispatched by the application
/// through `PluginRuntime::dispatch_command`, whose registration and
/// capability gates fail closed as usual. `delta` is the signed wheel step
/// (direction x lines, positive for wheel-up); the dispatch table carries it
/// under [`UI_SCROLL_DELTA_ARG`],
/// overwriting a declared entry of the same name.
#[derive(Debug, Clone, PartialEq)]
pub struct BandScrollRequest {
    /// Owning plugin id (the band that was hit).
    pub plugin_id: String,
    /// Command verb, unqualified (`scroll`, never `other-plugin:verb`).
    pub command: String,
    /// Declared `on_scroll.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Declared per-item region, when the wheel span declared one
    /// (issue #2248). Dispatch carries its id under [`UI_REGION_ARG`].
    pub region: Option<UiRegion>,
    /// Host-computed signed line step for this request.
    pub delta: i32,
    /// Presentation-captured origin (CTX-1332, C-D4).
    #[doc(hidden)]
    pub origin: Option<BandOrigin>,
    /// Monotonic admission sequence assigned when the gesture is queued.
    #[doc(hidden)]
    pub admission_seq: u64,
}

/// Converts declared scroll args plus the host-computed delta to the single
/// Lua table arg (CTX-1190, issue #2008).
///
/// Mirrors [`band_click_args_table`]: `dispatch_command` takes positional
/// args; a scroll carries one named table (`run({id = 3, delta = 1})`),
/// preserving declaration order with the region id (when declared) and
/// `delta` merged last. Declared `region`/`delta` entries are overwritten by
/// the host values, never duplicated.
#[must_use]
pub fn band_scroll_args_table(
    args: &[(String, ClickArg)],
    delta: i32,
    region: Option<&UiRegion>,
) -> LuaValue {
    let mut table: Vec<(LuaValue, LuaValue)> = args
        .iter()
        // `delta` is always host-owned. `region` is host-owned only when the
        // binding declared one; a legacy span keeps its own `region` arg.
        .filter(|(key, _)| key != UI_SCROLL_DELTA_ARG && (region.is_none() || key != UI_REGION_ARG))
        .map(|(key, value)| (LuaValue::String(key.clone()), click_arg_value(value)))
        .collect();
    if let Some(region) = region {
        table.push((
            LuaValue::String(UI_REGION_ARG.to_string()),
            LuaValue::String(region.id.clone()),
        ));
    }
    table.push((
        LuaValue::String(UI_SCROLL_DELTA_ARG.to_string()),
        LuaValue::Number(f64::from(delta)),
    ));
    LuaValue::Table(table)
}

/// Converts declared hover args plus the host-owned phase to the single Lua
/// table arg (issue #2411).
///
/// Mirrors [`band_click_args_table`]: `dispatch_command` takes positional
/// args; a hover carries one named table (`run({region = "clock", phase =
/// "enter"})`), preserving declaration order with the region id and phase
/// merged last. Both host keys overwrite declared entries of the same name
/// (the parser reserves their slots), so a dispatch can never exceed
/// `UI_CLICK_MAX_ARGS` and a plugin schema can never see a duplicate key.
#[must_use]
pub fn band_hover_args_table(
    args: &[(String, ClickArg)],
    region: &UiRegion,
    phase: BandHoverPhase,
) -> LuaValue {
    let mut table: Vec<(LuaValue, LuaValue)> = args
        .iter()
        .filter(|(key, _)| key != UI_HOVER_PHASE_ARG && key != UI_REGION_ARG)
        .map(|(key, value)| (LuaValue::String(key.clone()), click_arg_value(value)))
        .collect();
    table.push((
        LuaValue::String(UI_REGION_ARG.to_string()),
        LuaValue::String(region.id.clone()),
    ));
    table.push((
        LuaValue::String(UI_HOVER_PHASE_ARG.to_string()),
        LuaValue::String(phase.as_str().to_string()),
    ));
    LuaValue::Table(table)
}

/// Outcome of resolving the hoverable region at a display-cell column
/// (issue #2411).
///
/// Pure over one band, mirroring [`resolve_click_in_band`]: the hover binding
/// under the column wins (nearest declaration), an absent binding claims
/// nothing, and an unroutable command is denied loudly instead of silently
/// dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum BandHoverOutcome {
    /// A hover-bound span: report the transition to the owner.
    Target(BandHoverTarget),
    /// No span binds hover at the column.
    NoClaim,
    /// The binding is unroutable (foreign qualifier, empty verb).
    Denied,
}

/// A resolved hover target: owner, region identity, and verb
/// (issue #2411).
#[derive(Debug, Clone, PartialEq)]
pub struct BandHoverTarget {
    /// Owning plugin id (the band that was hovered).
    pub plugin_id: String,
    /// Command verb, unqualified.
    pub command: String,
    /// Declared `on_hover.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Stable region identity declared beside the binding.
    pub region: UiRegion,
}

/// Resolves a display-cell column into its hover outcome (issue #2411).
#[must_use]
pub fn resolve_hover_in_band(owner: &str, runs: &[BandRun], col: usize) -> BandHoverOutcome {
    let Some(run) = find_run_index(runs, col).and_then(|index| runs.get(index)) else {
        return BandHoverOutcome::NoClaim;
    };
    let Some(hover) = run.on_hover.as_ref() else {
        return BandHoverOutcome::NoClaim;
    };
    let Some(region) = hover.region.clone() else {
        // Unreachable: the parser refuses an anonymous hover binding.
        return BandHoverOutcome::NoClaim;
    };
    let Some(verb) = split_band_command(&hover.command, owner) else {
        return BandHoverOutcome::Denied;
    };
    BandHoverOutcome::Target(BandHoverTarget {
        plugin_id: owner.to_string(),
        command: verb,
        args: hover.args.clone(),
        region,
    })
}

/// Outcome of resolving a wheel column inside one band (CTX-1190, #2008).
#[derive(Debug, Clone, PartialEq)]
pub enum BandScrollOutcome {
    /// A scroll-bound span: queue the owner's declared command.
    Hit(BandScrollTarget),
    /// No span binds scroll at the column: route nothing.
    NoClaim,
    /// The binding is unroutable (foreign qualifier, empty verb): deny loudly.
    Denied,
}

/// A resolved scroll target: owner, verb, and declared args (CTX-1190).
///
/// The host-computed `delta` joins at queue time, so resolution stays pure
/// over one band and headless tests need no wheel state.
#[derive(Debug, Clone, PartialEq)]
pub struct BandScrollTarget {
    /// Owning plugin id (the band that was hit).
    pub plugin_id: String,
    /// Command verb, unqualified.
    pub command: String,
    /// Declared `on_scroll.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Declared per-item region, if any (issue #2248).
    pub region: Option<UiRegion>,
    /// Presentation-captured origin (CTX-1332, C-D4).
    #[doc(hidden)]
    pub origin: Option<BandOrigin>,
}

/// Resolves a flattened-text column into its scroll outcome (CTX-1190).
///
/// Pure over one band: finds the run owning `col` (display-cell column) and
/// splits its `on_scroll` command against `owner`, mirroring
/// [`resolve_click_in_band`]. Geometry (which band owns the row) lives on
/// [`Runtime::plugin_band_hit`]; the wheel step joins in
/// `Runtime::band_scroll`.
#[must_use]
pub fn resolve_scroll_in_band(owner: &str, runs: &[BandRun], col: usize) -> BandScrollOutcome {
    let Some(run) = find_run_index(runs, col).and_then(|index| runs.get(index)) else {
        return BandScrollOutcome::NoClaim;
    };
    let Some(scroll) = run.on_scroll.as_ref() else {
        return BandScrollOutcome::NoClaim;
    };
    let Some(verb) = split_band_command(&scroll.command, owner) else {
        return BandScrollOutcome::Denied;
    };
    BandScrollOutcome::Hit(BandScrollTarget {
        plugin_id: owner.to_string(),
        command: verb,
        args: scroll.args.clone(),
        region: run.region.clone(),
        origin: None,
    })
}

/// Pointer phase a band hover dispatch carries (issue #2411).
///
/// Two states only: the host never reports motion, coordinates, or dwell, so a
/// plugin cannot reconstruct pointer geometry from hover events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandHoverPhase {
    /// The pointer entered the declared region.
    Enter,
    /// The pointer left the declared region.
    Leave,
}

impl BandHoverPhase {
    /// Dispatch spelling carried under
    /// [`UI_HOVER_PHASE_ARG`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enter => UI_HOVER_PHASE_ENTER,
            Self::Leave => UI_HOVER_PHASE_LEAVE,
        }
    }
}

/// One Core-routed band hover transition (issue #2411).
///
/// Like [`BandClickRequest`] the plugin never sees pointer coordinates: only
/// the owning plugin, the declared command verb and args, the stable region
/// identity, and the [`BandHoverPhase`]. Hover carries no authority: the
/// application dispatches it through the normal registration/capability gates,
/// it never captures input, and it never sets `modal_active`.
#[derive(Debug, Clone, PartialEq)]
pub struct BandHoverRequest {
    /// Owning plugin id (the band that was hovered).
    pub plugin_id: String,
    /// Command verb, unqualified.
    pub command: String,
    /// Declared `on_hover.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Stable region identity declared beside the hover binding.
    pub region: UiRegion,
    /// Transition this dispatch reports.
    pub phase: BandHoverPhase,
    /// Presentation-captured origin (CTX-1332 C-D4; issue #2411).
    ///
    /// Stamped from the last presented frame like a click or scroll, so the
    /// cold pump can refuse a transition whose band was replaced, moved, or
    /// reflowed after it was queued. A request without one is test-only and
    /// is refused by the application, so hover can never mint a stale action.
    #[doc(hidden)]
    pub origin: Option<BandOrigin>,
}

/// One live hover state: which band item the pointer is on (issue #2411).
///
/// The declared binding is captured at enter so the paired leave still names
/// the same command and region after a content update replaced the tree.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BandHoverState {
    edge: BandEdge,
    visible_index: usize,
    plugin_id: String,
    region: UiRegion,
    command: String,
    args: Vec<(String, ClickArg)>,
    /// Display-cell column inside the band that entered the region. Re-probing
    /// this column against the live tree is how a band replacement or a
    /// content update proves the hover still names a real item.
    col: usize,
    /// Presented origin captured at enter.
    ///
    /// The enter is queued only once this names a current presented frame;
    /// `leave` is never fenced (it hides/cleans up and grants no authority).
    origin: Option<BandOrigin>,
    /// Whether the `enter` transition reached the plugin runtime.
    ///
    /// A deferred enter (band newer than the last presented frame) stays
    /// unannounced until the cold phase re-derives it, so the app never
    /// anchors a tooltip for an item the plugin has not heard about, and a
    /// presented-frame refresh never re-fires a delivered enter.
    announced: bool,
}

impl BandHoverState {
    /// Transition identity: two states are the same item exactly when these
    /// agree, so re-declaring the same region does not re-fire enter.
    fn same_item(&self, other: &Self) -> bool {
        self.edge == other.edge
            && self.visible_index == other.visible_index
            && self.plugin_id == other.plugin_id
            && self.region == other.region
    }

    /// The `leave` dispatch for this item.
    fn leave_request(&self) -> BandHoverRequest {
        BandHoverRequest {
            plugin_id: self.plugin_id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            region: self.region.clone(),
            phase: BandHoverPhase::Leave,
            origin: self.origin.clone(),
        }
    }

    /// The `enter` dispatch for this item.
    fn enter_request(&self) -> BandHoverRequest {
        BandHoverRequest {
            plugin_id: self.plugin_id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            region: self.region.clone(),
            phase: BandHoverPhase::Enter,
            origin: self.origin.clone(),
        }
    }
}

/// Outcome of resolving the hoverable region under the pointer
/// (issue #2411).
#[derive(Debug, Clone, PartialEq)]
enum HoverProbe {
    /// The pointer is on a declared hover region whose command routes.
    Target(BandHoverState),
    /// The pointer is on a hover region whose command is unroutable.
    Denied,
    /// The pointer is off every hover region.
    None,
}

/// Maximum queued band hover transitions (issue #2411).
///
/// One entry per distinct region and phase, so the cap is the same bounded
/// per-frame work the accepted `UI_HOVER_QUEUE_MAX` names. Overflow drops the
/// newest transition fail-closed and counts
/// [`BandHostStats::hover_queue_drops`]; the next pointer motion re-derives
/// the live target, so a dropped edge event cannot wedge the hover state.
pub const BAND_HOVER_QUEUE_MAX: usize = bitty_lua::ui::UI_HOVER_QUEUE_MAX;

/// Window-cell anchor for the focusable overlay surface (issue #2411).
///
/// A tooltip overlay can be placed against a live band item instead of the
/// centered modal position. The anchor carries no absolute pointer data: it
/// names the band (edge + visible index) and the item's Core-derived column
/// span, so the paint path resolves the row live and a reflow can never place
/// the surface against stale geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandOverlayAnchor {
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible band index on that edge.
    pub visible_index: usize,
    /// First display-cell column of the anchored region (inclusive).
    pub start_col: usize,
    /// One past the last display-cell column (exclusive).
    pub end_col: usize,
}

impl BandOverlayAnchor {
    /// Anchors to one live band item region.
    #[must_use]
    pub fn from_region(region: &BandItemRegion) -> Self {
        Self {
            edge: region.edge,
            visible_index: region.visible_index,
            start_col: region.start_col,
            end_col: region.end_col,
        }
    }
}

/// Press-side band target stored by [`Runtime::band_press`] (CTX-0946 C1).///
/// Standard button semantics: a release routes only onto the same target
/// the press armed (band, run, and resolved request all equal). Anything
/// else — a drag onto another span or band, or content that changed
/// mid-gesture — counts unclaimed (or denied, when the release itself is
/// denied on the same target the press armed).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PressedBandTarget {
    /// The press armed a claimed item: band, item key, resolved request.
    Hit {
        edge: BandEdge,
        visible_index: usize,
        key: BandTargetKey,
        button: BandButton,
        request: Box<BandClickRequest>,
    },
    /// The press landed on an unclaimed span or past the text.
    NoClaim,
    /// The press landed on an unroutable claim (foreign qualifier,
    /// region overflow, ...).
    Denied,
}

/// Item identity a band press armed (issue #2248).
///
/// A declared region pairs by identity, so a press on one leaf of a merged
/// region and a release on another leaf of the same region is still the same
/// item. A legacy span pairs by run index, preserving the pre-2248 rule that
/// a drag across spans fires nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BandTargetKey {
    /// Declared per-item region identity.
    Region(UiRegion),
    /// Legacy span-only target (run index).
    Span(usize),
}

/// Cursor-side band scroll classification behind [`Runtime::band_scroll`]
/// (CTX-1190, issue #2008).
///
/// A wheel is a single event (no press arming), so resolution maps straight
/// onto the consume/fall-through decision: only `Hit` consumes the tick.
#[derive(Debug, Clone, PartialEq)]
enum ScrollTargetOutcome {
    /// A scroll-bound span: queue the target with the tick's delta.
    Hit(BandScrollTarget),
    /// Off every band row (or no cursor): fall through silently.
    OffBand,
    /// On a band row but past the text or on a span without `on_scroll`:
    /// fall through, counted.
    Unclaimed,
    /// On a span with an unroutable scroll binding: fall through, counted.
    Denied,
    /// The band declared more than [`BAND_MAX_REGIONS`] regions: refuse
    /// every wheel on it (fail-closed) and count the overflow.
    Overflow,
}

/// Whether band rows collide (duplicate rows).
///
/// Backstop behind the budget math (which keeps rows disjoint by
/// construction): paint and hit-test fail closed on `true` with a
/// diagnostic instead of routing or painting either claimant.
///
/// CTX-0979: Core draws no workspace display, so there is no Core bar row
/// to collide with; only duplicate plugin rows violate.
#[must_use]
pub fn band_rows_overlap(rows: &[u16]) -> bool {
    let mut sorted = rows.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).any(|pair| pair[0] == pair[1])
}

/// Headless host statistics for band routing and paint (CTX-0946).
///
/// Saturating counters; the application surfaces dispatch failures loudly
/// per gesture (user-paced, never a hot loop).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BandHostStats {
    /// Clicks routed to the drain queue.
    pub clicks_routed: u64,
    /// Clicks denied (foreign qualifier, malformed command, row overlap).
    pub clicks_denied: u64,
    /// Presses/releases on band rows with no claim (nothing dispatched).
    pub clicks_unclaimed: u64,
    /// Band paints skipped fail-closed on a geometry violation.
    pub paint_violations: u64,
    /// Unknown theme tokens substituted with the span default.
    pub unknown_tokens: u64,
    /// Click requests dropped on a full drain queue.
    pub queue_drops: u64,
    /// Scrolls routed to the drain queue (CTX-1190, issue #2008).
    pub scrolls_routed: u64,
    /// Wheel ticks merged into the trailing queued scroll (CTX-1190).
    pub scrolls_coalesced: u64,
    /// Scrolls denied (foreign qualifier, malformed command) (CTX-1190).
    pub scrolls_denied: u64,
    /// Wheels on band rows with no scroll binding (CTX-1190).
    pub scrolls_unclaimed: u64,
    /// Scroll requests dropped on a full drain queue (CTX-1190).
    pub scroll_queue_drops: u64,
    /// Band gestures refused because the band declared more than
    /// [`BAND_MAX_REGIONS`] distinct regions (issue #2248).
    pub region_overflows: u64,
    /// Hover transitions queued for the cold phase (issue #2411).
    pub hovers_routed: u64,
    /// Hover transitions that collapsed into an already-queued transition of
    /// the same region (issue #2411).
    pub hovers_coalesced: u64,
    /// Hover `enter` transitions held back until the band's frame is
    /// presented (issue #2411).
    pub hovers_deferred: u64,
    /// Hover transitions dropped on a full queue (issue #2411).
    pub hover_queue_drops: u64,
    /// Hover bindings refused because their command is unroutable
    /// (issue #2411).
    pub hovers_denied: u64,
}

/// A resolved band hit: which visible band, and which text column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandHit {
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible index on the edge (hidden bands take no row).
    pub visible_index: usize,
    /// Window row the band paints (window cells).
    pub row: u16,
    /// Display-cell column inside the band line, measured with the same
    /// [`char_cell_width`](bitty_term_state::char_cell_width) rules as
    /// [`BandRun::start_col`]/[`BandRun::end_col`]. For a row band this is the
    /// window column (the band spans the window width); for a column band it is
    /// measured from the band's own left edge, the same in-band unit
    /// [`BandItemRegion::start_col`] uses. This is the unit every hit-test
    /// consumer uses (issue #2248, issue #3171).
    pub col: usize,
}

/// One band content line under a resolved [`BandCellHit`] (issue #3171).
///
/// The shared narrowing target for click, scroll, and hover: the owning plugin,
/// the content line's window row, the in-band column, and the flattened spans
/// of that line — the same spans paint clips and per-item regions chart.
struct BandLineAt {
    edge: BandEdge,
    visible_index: usize,
    plugin_id: String,
    row: u16,
    col: usize,
    runs: Vec<BandRun>,
}

/// A resolved band cell: which visible band owns the pointer, and the window
/// cell the pointer is on.
///
/// Both axes resolve to this (issue #3171): a row band is one window row across
/// the window width, a column band is its granted column range across the
/// row-carved container's rows. Press consumption uses the whole granted rect —
/// including the unclaimed trailing cells past the text — so a band press can
/// never fall through to selection or capture; release resolution narrows
/// through [`Runtime::plugin_band_hit`], whose text-width guard keeps a
/// past-text release at `NoClaim` instead of saturating into the final span's
/// claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandCellHit {
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible index on the edge (hidden bands take no index).
    pub visible_index: usize,
    /// Window row the pointer is on (window cells).
    pub row: u16,
    /// Window column the pointer is on (window cells).
    pub column: u16,
}

/// One granted column band, in stacking order (issue #3171).
///
/// Produced by [`Runtime::column_band_plan`], the single allocation source for
/// both vertical edges: the column budget, the exclusive-zone carve, the paint
/// plan, hit-testing, item regions, and published origins all read it, so a
/// band cannot be reserved at one width and painted or routed at another.
pub(super) struct ColumnBandRect {
    /// Edge owning the band.
    pub(super) edge: BandEdge,
    /// Visible index on the edge (hidden bands take no index).
    pub(super) visible_index: usize,
    /// First window column of the granted range.
    pub(super) column: u16,
    /// Granted width in cells: the band's content width when it fits its share,
    /// the remaining share when it does not.
    pub(super) width: u16,
}

impl Runtime {
    fn row_band_selection(&self) -> (Vec<&BandContent>, Vec<&BandContent>) {
        let visible_for = |edge| {
            self.chrome_bands
                .edge(edge)
                .iter()
                .filter(|band| band_is_visible(&band.root))
                .collect::<Vec<_>>()
        };
        let top = visible_for(BandEdge::Top);
        let bottom = visible_for(BandEdge::Bottom);
        let total = top.len().saturating_add(bottom.len());
        if total == 0 {
            return (top, bottom);
        }
        let window_rows = usize::from(self.window_cells().height);
        let floor = usize::from(self.chrome_min_rows());
        let Some(spare) = window_rows.checked_sub(floor) else {
            return (Vec::new(), Vec::new());
        };
        if spare == 0 {
            return (Vec::new(), Vec::new());
        }
        if total <= spare {
            return (top, bottom);
        }
        // Assign per-plugin round-robin ranks so multi-band plugins (such as
        // a multi-row bottom panel) shed duplicate rows before evicting primary
        // bands like `bar`, while keeping deterministic stacking order among
        // retained bands. When even one primary band per distinct plugin
        // cannot fit inside `spare`, degrade all horizontal bands cleanly.
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut candidates: Vec<(usize, BandEdge, usize)> = Vec::with_capacity(total);
        for (edge, bands) in [(BandEdge::Top, &top), (BandEdge::Bottom, &bottom)] {
            for (index, band) in bands.iter().enumerate() {
                let rank = counts
                    .entry(band.plugin_id.as_str())
                    .and_modify(|c| *c += 1)
                    .or_insert(0);
                candidates.push((*rank, edge, index));
            }
        }
        if counts.len() > spare {
            return (Vec::new(), Vec::new());
        }
        candidates.sort_by_key(|&(rank, _, _)| rank);
        let mut keep_top = vec![false; top.len()];
        let mut keep_bottom = vec![false; bottom.len()];
        for &(_, edge, index) in candidates.iter().take(spare) {
            match edge {
                BandEdge::Top => keep_top[index] = true,
                BandEdge::Bottom => keep_bottom[index] = true,
                BandEdge::Left | BandEdge::Right => {}
            }
        }
        let kept_top = top
            .into_iter()
            .zip(keep_top)
            .filter_map(|(band, keep)| keep.then_some(band))
            .collect();
        let kept_bottom = bottom
            .into_iter()
            .zip(keep_bottom)
            .filter_map(|(band, keep)| keep.then_some(band))
            .collect();
        (kept_top, kept_bottom)
    }

    /// Visible (non-empty-text) bands on `edge` within the live budget, in
    /// stacking order.
    ///
    /// Hidden bands take no stacking row (CTX-0925 item 3): every consumer
    /// — row assignment, paint, hit-test, and the exclusive-zone budget —
    /// reads this list, so a hidden band can never own geometry.
    #[must_use]
    pub fn visible_edge_bands(&self, edge: BandEdge) -> Vec<&BandContent> {
        match edge {
            BandEdge::Top => self.row_band_selection().0,
            BandEdge::Bottom => self.row_band_selection().1,
            BandEdge::Left | BandEdge::Right => self
                .chrome_bands
                .edge(edge)
                .iter()
                .filter(|band| band_is_visible(&band.root))
                .collect(),
        }
    }

    /// Visible band rows reserved on horizontal `edge` (CTX-0946 C3).
    ///
    /// Counts visible bands only, clamped by the degradation budget
    /// ([`Self::plugin_band_budget`]): `0` when the window cannot hold the
    /// bands above the content floor, or for a vertical (column) edge — column
    /// bands reserve columns, see [`Self::visible_band_column_count`].
    #[must_use]
    pub fn visible_band_count(&self, edge: BandEdge) -> u16 {
        let (top, bottom) = self.plugin_band_budget();
        match edge {
            BandEdge::Top => top,
            BandEdge::Bottom => bottom,
            BandEdge::Left | BandEdge::Right => 0,
        }
    }

    /// Visible vertical bands on `edge`, clamped by the column plan
    /// (issue #3165, issue #3171).
    ///
    /// The column mirror of [`Self::visible_band_count`]: the number of
    /// visible (non-empty-text) bands on `left`/`right` the column budget
    /// could grant columns to, or `0` for a horizontal (row) edge or a window
    /// too narrow for any share at all. The granted widths themselves come from
    /// the host's column grant plan (`column_band_plan`).
    #[must_use]
    pub fn visible_band_column_count(&self, edge: BandEdge) -> u16 {
        if edge.axis() != BandAxis::Column {
            return 0;
        }
        let count = self
            .column_band_plan()
            .iter()
            .filter(|band| band.edge == edge)
            .count();
        u16::try_from(count).unwrap_or(u16::MAX)
    }

    /// Degraded-visible band budget per horizontal edge (CTX-0946 C3).
    ///
    /// Derived from [`Self::row_band_selection`] so paint, hit-test, and the
    /// content container never disagree. Multi-band plugins shed excess rows
    /// first when `want > spare`; `(0, 0)` when there are zero spare rows or
    /// even one row per distinct plugin cannot fit above [`Self::chrome_min_rows`].
    #[must_use]
    pub fn plugin_band_budget(&self) -> (u16, u16) {
        let (top, bottom) = self.row_band_selection();
        (
            u16::try_from(top.len()).unwrap_or(u16::MAX),
            u16::try_from(bottom.len()).unwrap_or(u16::MAX),
        )
    }

    /// Width budget of the visible vertical bands per edge, in window columns
    /// (issue #3165, issue #3171).
    ///
    /// The column mirror of [`Self::plugin_band_budget`]: `(left, right)` are
    /// the columns the host's column grant plan (`column_band_plan`) gave the
    /// visible bands on each side (their content widths when those fit the
    /// side's share), or `(0, 0)` when no side was granted anything. The budget
    /// is derived from that plan rather than recomputed, so the reservation and
    /// the paint plan are one number and a band is never reserved but unpainted
    /// (or painted but unreserved).
    #[must_use]
    pub fn plugin_band_column_budget(&self) -> (u16, u16) {
        let plan = self.column_band_plan();
        let width = |edge| {
            plan.iter()
                .filter(|band| band.edge == edge)
                .map(|band| band.width)
                .fold(0u16, u16::saturating_add)
        };
        (width(BandEdge::Left), width(BandEdge::Right))
    }

    /// Granted column bands, edge-major in stacking order (issue #3171).
    ///
    /// One allocation policy for both vertical edges. Each side is capped at an
    /// equal part of what the window can spare above
    /// [`Self::chrome_min_cols`] and above the
    /// [`BAND_COLUMN_CONTENT_SHARE_DIVISOR`] content share, so the content area
    /// always keeps at least half the window; bands stack outward-in from the
    /// window edge, and the band that crosses the cap is granted the remainder
    /// instead of vanishing. A side granted zero columns (a window with no
    /// spare column at all) paints, reserves, and routes nothing — the same
    /// degradation a row band takes in a window too short for it — so a band is
    /// either served, possibly clipped to its share, or the window itself is
    /// below the content floor.
    pub(super) fn column_band_plan(&self) -> Vec<ColumnBandRect> {
        let window = self.window_cells();
        if window.width == 0 {
            return Vec::new();
        }
        let wants: Vec<(BandEdge, Vec<u16>)> = [BandEdge::Left, BandEdge::Right]
            .into_iter()
            .map(|edge| {
                let widths = self
                    .visible_edge_bands(edge)
                    .iter()
                    .map(|band| column_band_width(&band.root))
                    .collect();
                (edge, widths)
            })
            .collect();
        let sides = u32::try_from(
            wants
                .iter()
                .filter(|(_, widths)| !widths.is_empty())
                .count(),
        )
        .unwrap_or(u32::MAX);
        if sides == 0 {
            return Vec::new();
        }
        let content = u32::from(self.chrome_min_cols())
            .max(u32::from(window.width).div_ceil(u32::from(BAND_COLUMN_CONTENT_SHARE_DIVISOR)));
        let Some(share) = u32::from(window.width).checked_sub(content) else {
            return Vec::new();
        };
        let per_side = share / sides;
        let mut plan = Vec::new();
        for (edge, widths) in wants {
            let mut offset = 0u16;
            for (index, want) in widths.iter().enumerate() {
                if *want == 0 {
                    continue;
                }
                let remaining = per_side.saturating_sub(u32::from(offset));
                let granted = (*want).min(u16::try_from(remaining).unwrap_or(u16::MAX));
                if granted == 0 {
                    break;
                }
                let Some(column) = ChromeBands::band_column(edge, offset, granted, window.width)
                else {
                    break;
                };
                plan.push(ColumnBandRect {
                    edge,
                    visible_index: index,
                    column,
                    width: granted,
                });
                offset = offset.saturating_add(granted);
                // A band clipped to its share ends the stack: the inward bands
                // have no columns left, and painting them over the content
                // would defeat the share.
                if granted < *want {
                    break;
                }
            }
        }
        plan
    }

    /// Granted rect of visible band `index` on `edge`, in window cells
    /// (issue #3171): `(row, column, width, height)`.
    ///
    /// The single geometry source both axes resolve through — paint,
    /// hit-testing, per-item regions, and published origins all read it — so a
    /// column band's granted width is the same number that reserves its
    /// columns, clips its paint, and bounds its item rects. A row band is one
    /// window row across the window width (the window column origin is `0`
    /// because it spans the width); a column band spans the row-carved
    /// container's rows at its granted column range. `None` for a hidden band,
    /// an out-of-range index, or a band the budget granted nothing.
    fn band_paint_rect(&self, edge: BandEdge, index: usize) -> Option<(u16, u16, u16, u16)> {
        let window = self.window_cells();
        match edge.axis() {
            BandAxis::Row => Some((self.visible_band_row(edge, index)?, 0, window.width, 1)),
            BandAxis::Column => {
                let granted = self
                    .column_band_plan()
                    .into_iter()
                    .find(|band| band.edge == edge && band.visible_index == index)?;
                let area = self.band_row_container();
                Some((area.y, granted.column, granted.width, area.height))
            }
        }
    }

    /// Window row painted by visible band `index` on `edge` (CTX-0946 C3).
    ///
    /// `index` counts visible bands only (hidden bands take no row).
    /// `None` for a vertical edge, an out-of-range index, or a band that
    /// does not fit (degraded away by [`Self::plugin_band_budget`]).
    #[must_use]
    pub fn visible_band_row(&self, edge: BandEdge, index: usize) -> Option<u16> {
        if u64::try_from(index).unwrap_or(u64::MAX) >= u64::from(self.visible_band_count(edge)) {
            return None;
        }
        self.plugin_band_row(edge, index)
    }

    /// Leftmost window column of visible vertical band `index` on `edge`
    /// (issue #3165): the column mirror of [`Self::visible_band_row`].
    ///
    /// `index` counts visible bands only and stacks from the window edge
    /// inward (index `0` outermost), so the band starts inward of the granted
    /// widths of every visible band held outward of it on the same edge.
    /// `None` for a horizontal edge, an out-of-range index, a width of zero,
    /// or a band the column plan granted nothing.
    #[must_use]
    pub fn visible_band_column(&self, edge: BandEdge, index: usize) -> Option<u16> {
        self.column_band_plan()
            .into_iter()
            .find(|band| band.edge == edge && band.visible_index == index)
            .map(|band| band.column)
    }

    /// Layout container minus the budgeted horizontal bands (CTX-0946 C3).
    ///
    /// CTX-0979: Core draws no workspace display, so the container starts
    /// from the full window grid; this carves the plugin exclusive zone out
    /// of it. Falls back to the full window when the budget hides every
    /// band, so the no-plugin frame is byte-identical to before. Saturating
    /// and total.
    ///
    /// Issue #3165: this is the row carve only — the columns the vertical
    /// bands own are removed by [`Self::band_exclusive_container`], and a
    /// column band spans exactly this rect's rows.
    #[must_use]
    pub fn band_row_container(&self) -> bitty_ui::Rect {
        let window = self.chrome_layout().container;
        let (top, bottom) = self.plugin_band_budget();
        if top == 0 && bottom == 0 {
            return window;
        }
        let y = window.y.saturating_add(top);
        let end = window
            .y
            .saturating_add(window.height)
            .saturating_sub(bottom);
        let height = end.saturating_sub(y);
        bitty_ui::Rect::new(window.x, y, window.width, height)
    }

    /// Layout container minus the budgeted plugin bands (CTX-0946 C3,
    /// issue #3165 for the vertical edges).
    ///
    /// Horizontal bands reserve rows ([`Self::band_row_container`]) and the
    /// vertical bands reserve their column ranges from that rect, so a
    /// `left`/`right` band shrinks the container width exactly as a
    /// `top`/`bottom` band shrinks its height. Falls back to the row carve
    /// when the column budget hides every vertical band, so a window with no
    /// vertical mount is byte-identical to before. Saturating and total.
    #[must_use]
    pub fn band_exclusive_container(&self) -> bitty_ui::Rect {
        let window = self.band_row_container();
        let (left, right) = self.plugin_band_column_budget();
        if left == 0 && right == 0 {
            return window;
        }
        let x = window.x.saturating_add(left);
        let end = window.x.saturating_add(window.width).saturating_sub(right);
        let width = end.saturating_sub(x);
        bitty_ui::Rect::new(x, window.y, width, window.height)
    }

    /// Band rows currently painted, for overlap fail-closed checks.
    fn painted_band_rows(&self) -> Vec<u16> {
        let (top, bottom) = self.plugin_band_budget();
        let mut rows = Vec::with_capacity(usize::from(top).saturating_add(usize::from(bottom)));
        for index in 0..top {
            if let Some(row) = self.visible_band_row(BandEdge::Top, usize::from(index)) {
                rows.push(row);
            }
        }
        for index in 0..bottom {
            if let Some(row) = self.visible_band_row(BandEdge::Bottom, usize::from(index)) {
                rows.push(row);
            }
        }
        rows
    }

    /// Whether the painted band rows violate exclusive geometry.
    ///
    /// True on any duplicate row or any band row inside the layout
    /// container, and (issue #3171) on any granted column band whose range
    /// reaches the layout container — the column mirror of the row backstop.
    /// Paint and hit-test fail closed on `true` with a diagnostic; the budget
    /// math keeps this unreachable in practice.
    ///
    /// CTX-0979: no Core bar row exists to collide with.
    #[must_use]
    pub fn band_geometry_violated(&self) -> bool {
        let rows = self.painted_band_rows();
        if band_rows_overlap(&rows) {
            return true;
        }
        let container = self.container;
        let container_end = container.y.saturating_add(container.height);
        if rows
            .iter()
            .any(|row| *row >= container.y && *row < container_end)
        {
            return true;
        }
        let container_end_x = container.x.saturating_add(container.width);
        self.column_band_plan().iter().any(|band| {
            let end = band.column.saturating_add(band.width);
            band.column < container_end_x && end > container.x
        })
    }

    /// Maps physical `pos` onto a painted band cell, on either axis
    /// (CTX-0946 C1, issue #3171).
    ///
    /// Rect-wide on both axes: for a row band any column inside the window
    /// width hits (including the unclaimed trailing region past the text), and
    /// for a column band any row inside the band's granted column range hits.
    /// Press consumption uses this so a band press can never fall through to
    /// selection or capture. Both axes resolve through the host's granted band
    /// rect (`band_paint_rect`), the same geometry the paint path uses (no
    /// second source). Returns `None` outside every painted band rect, past the
    /// window, or under any geometry violation (fail-closed).
    #[must_use]
    pub fn plugin_band_cell_hit(&self, pos: CursorPosition) -> Option<BandCellHit> {
        if !pos.x.is_finite() || !pos.y.is_finite() {
            return None;
        }
        if self.band_geometry_violated() {
            return None;
        }
        let live = self.live_cell_metrics();
        if live.width == 0 || live.height == 0 {
            return None;
        }
        let window = self.window_cells();
        if window.width == 0 || window.height == 0 {
            return None;
        }
        let pad = f64::from(self.window_padding_physical());
        let cell_w = f64::from(live.width);
        let cell_h = f64::from(live.height);
        let origin_x = pad + f64::from(window.x) * cell_w;
        // The window cell the pointer is on, in the same window-relative frame
        // [`Self::band_paint_rect`] reports.
        let cell_col = (pos.x - origin_x) / cell_w;
        let cell_row = (pos.y - pad) / cell_h;
        if !(0.0..f64::from(window.width)).contains(&cell_col)
            || !(0.0..f64::from(window.height)).contains(&cell_row)
        {
            return None;
        }
        let column = u16::try_from(cell_col.floor() as u64).ok()?;
        let row = u16::try_from(cell_row.floor() as u64).ok()?;
        for edge in BAND_EDGES {
            let count = match edge.axis() {
                BandAxis::Row => self.visible_band_count(edge),
                BandAxis::Column => self.visible_band_column_count(edge),
            };
            for index in 0..usize::from(count) {
                let Some((band_row, band_column, width, height)) =
                    self.band_paint_rect(edge, index)
                else {
                    continue;
                };
                if column < band_column || column >= band_column.saturating_add(width) {
                    continue;
                }
                if row < band_row || row >= band_row.saturating_add(height) {
                    continue;
                }
                return Some(BandCellHit {
                    edge,
                    visible_index: index,
                    row,
                    column,
                });
            }
        }
        None
    }

    /// Narrows a resolved [`BandCellHit`] onto the band content line under the
    /// pointer (issue #3171).
    ///
    /// The axis-general form of the pre-#3171 row-only narrowing: a row band has
    /// one line (its window row) whose in-band column is the window column; a
    /// column band has one line per root child, the line is the pointer's row
    /// offset inside the band, and the column is measured from the band's own
    /// left edge. Those are exactly the spans paint clips and per-item regions
    /// chart, so click, scroll, and hover resolve through one mapping. `None`
    /// past the painted text width (the paint path clips text to the band
    /// width, so the guard mirrors it), on an empty line, or for a band the
    /// budget granted nothing.
    fn band_line_at(&self, cell: &BandCellHit) -> Option<BandLineAt> {
        let (origin_row, origin_column, width, height) =
            self.band_paint_rect(cell.edge, cell.visible_index)?;
        let bands = self.visible_edge_bands(cell.edge);
        let band = bands.get(cell.visible_index)?;
        let runs = match cell.edge.axis() {
            BandAxis::Row => flatten_band_runs_in_width(&band.root, usize::from(width)).1,
            BandAxis::Column => {
                let offset = usize::from(cell.row.saturating_sub(origin_row));
                let rows = column_band_rows(&band.root, usize::from(height));
                rows.get(offset)?.1.clone()
            }
        };
        let col = usize::from(cell.column.saturating_sub(origin_column));
        let width_cells = runs
            .iter()
            .map(|run| run.end_col)
            .max()
            .unwrap_or(0)
            .min(usize::from(width));
        if col >= width_cells {
            return None;
        }
        Some(BandLineAt {
            edge: cell.edge,
            visible_index: cell.visible_index,
            plugin_id: band.plugin_id.clone(),
            row: cell.row,
            col,
            runs,
        })
    }

    /// Maps physical `pos` onto a claimed band text column, if any
    /// (CTX-0946 C1, issue #3171).
    ///
    /// Release resolution over [`Self::plugin_band_cell_hit`]: the band rect
    /// must hit, and the column must land inside the painted text width of the
    /// line under the pointer (the paint path clips text to the band width, so
    /// the guard mirrors it). Returns `None` past the text width — the caller
    /// counts that release as unclaimed, never as the final span's owner.
    #[must_use]
    pub fn plugin_band_hit(&self, pos: CursorPosition) -> Option<BandHit> {
        let cell = self.plugin_band_cell_hit(pos)?;
        let line = self.band_line_at(&cell)?;
        Some(BandHit {
            edge: line.edge,
            visible_index: line.visible_index,
            row: line.row,
            col: line.col,
        })
    }

    /// Emits the bounded per-item region table for one visible band
    /// (issue #2248, issue #3171).
    ///
    /// `edge`/`visible_index` address the band in the same stacking order
    /// paint, hit-test, and origins use; the band's granted rect comes from the
    /// host's single band-geometry source (`band_paint_rect`). Rectangles are
    /// display-cell columns measured from the band's own left edge and clipped
    /// to its granted width — one row band line, or one line per column-band
    /// root child. Regions are derived on demand from the current tree, so a
    /// remount or reflow can never expose stale geometry: the next call reads
    /// the new tree. A band above [`BAND_MAX_REGIONS`] returns
    /// [`BandRegionError::Overflow`] and routes nothing (fail-closed). No
    /// plugin VM is entered; this is a pure data walk.
    ///
    /// # Errors
    ///
    /// [`BandRegionError::MissingBand`] when no band occupies the index with a
    /// granted rect (hidden, out of range, or granted no columns), or
    /// [`BandRegionError::Overflow`] for an over-cap band.
    pub fn band_item_regions(
        &self,
        edge: BandEdge,
        visible_index: usize,
    ) -> Result<Vec<BandItemRegion>, BandRegionError> {
        let (row, _column, width, height) = self
            .band_paint_rect(edge, visible_index)
            .ok_or(BandRegionError::MissingBand)?;
        let bands = self.visible_edge_bands(edge);
        let band = bands
            .get(visible_index)
            .ok_or(BandRegionError::MissingBand)?;
        match edge.axis() {
            BandAxis::Row => band_regions_for(band, edge, visible_index, row, usize::from(width)),
            BandAxis::Column => {
                column_band_regions_for(band, edge, visible_index, row, width, usize::from(height))
            }
        }
    }

    /// Routes a press on a plugin band to chrome (CTX-0946 C1,
    /// issue #2248, issue #3171).
    ///
    /// Returns `true` (consume the press: no focus move, no selection, no
    /// capture report, and no clipboard paste) when the last-known cursor
    /// sits anywhere on a painted band rect — including the unclaimed
    /// trailing cells past the text — arming the one-shot
    /// [`Self::band_release_swallow`] plus the stored press target so the
    /// paired release routes only onto the same item (standard button
    /// semantics). Left, right, and middle presses are consumed; back,
    /// forward, and other buttons fall through unchanged. Left with Shift
    /// still forces the selection path (the CTX-0181 accessibility escape);
    /// Shift does not re-enable clipboard paste on chrome for other buttons.
    pub(super) fn band_press(&mut self, mouse_button: MouseButton) -> bool {
        let Some(button) = BandButton::from_mouse(mouse_button) else {
            return false;
        };
        if self.shift_pressed && button == BandButton::Left {
            return false;
        }
        let Some(pos) = self.last_cursor else {
            return false;
        };
        let Some(cell) = self.plugin_band_cell_hit(pos) else {
            return false;
        };
        // A different button while one is armed is still chrome *on a band
        // row*: consume it without disturbing the armed gesture, and remember
        // its release so the pair never falls through to clipboard/capture.
        // A repeated press of the *same* button is a lost-release recovery:
        // fall through and re-arm instead of wedging the band forever.
        if let Some(armed) = self.band_press_button {
            if armed != button {
                if self.band_extra_press.is_none() {
                    self.band_extra_press = Some(button);
                }
                return true;
            }
        }
        if self.band_region_overflow(cell.edge, cell.visible_index) {
            self.band_stats.region_overflows = self.band_stats.region_overflows.saturating_add(1);
        }
        self.band_press_target = Some(self.band_target_at(&cell, button));
        self.band_press_button = Some(button);
        self.band_release_swallow = true;
        true
    }

    /// Whether a band's region table overflowed its bound (issue #2248).
    fn band_region_overflow(&self, edge: BandEdge, visible_index: usize) -> bool {
        matches!(
            self.band_item_regions(edge, visible_index),
            Err(BandRegionError::Overflow { .. })
        )
    }

    /// Resolves a press or release position into its stored-target form.
    ///
    /// Shared by [`Self::band_press`] and [`Self::band_release`] so both
    /// sides classify identically: past-text is `NoClaim` (the width guard
    /// in [`Self::plugin_band_hit`]). A declared region resolves to that
    /// item; a bare interactive span keeps the legacy span target. Both axes
    /// resolve through [`Self::band_line_at`], so a column band's press
    /// resolves on its own content line with the same rules a row band uses
    /// (issue #3171).
    fn band_target_at(&self, cell: &BandCellHit, button: BandButton) -> PressedBandTarget {
        let Some(line) = self.band_line_at(cell) else {
            return PressedBandTarget::NoClaim;
        };
        let owner = line.plugin_id;
        // Issue #2248: a declared region owns its rect. Overflow refuses the
        // whole band (fail-closed) rather than routing a partial table.
        let regions = match self.band_item_regions(line.edge, line.visible_index) {
            Ok(regions) => regions,
            Err(BandRegionError::MissingBand) => return PressedBandTarget::NoClaim,
            Err(BandRegionError::Overflow { .. }) => return PressedBandTarget::Denied,
        };
        let declared =
            resolve_item_region(&regions, line.row, line.col).map(|item| item.region.clone());
        let outcome = match declared.as_ref() {
            Some(region) => resolve_click_for_region(&owner, &line.runs, region, line.col, button),
            None => resolve_click_in_band(&owner, &line.runs, line.col, button),
        };
        match outcome {
            BandColumnOutcome::Hit(request) => {
                let key = match request.region.clone() {
                    Some(region) => BandTargetKey::Region(region),
                    None => {
                        let Some(run) = find_run_index(&line.runs, line.col) else {
                            // Unreachable (a Hit always owns a run); fail closed.
                            return PressedBandTarget::NoClaim;
                        };
                        BandTargetKey::Span(run)
                    }
                };
                // CTX-1332 C-D4: capture the presentation-published origin now
                // (the press and release both resolve against the same live
                // band); the cold pump refuses a request without one.
                let mut request = request;
                request.origin = self
                    .published_band_origin(line.edge, line.visible_index)
                    .cloned();
                PressedBandTarget::Hit {
                    edge: line.edge,
                    visible_index: line.visible_index,
                    key,
                    button,
                    request: Box::new(request),
                }
            }
            BandColumnOutcome::Denied => PressedBandTarget::Denied,
            BandColumnOutcome::NoClaim => PressedBandTarget::NoClaim,
        }
    }

    /// Drops an armed band press without routing (focus loss pairs with the
    /// release-swallow clear in [`Runtime::set_focused`](super::input)).
    pub(super) fn clear_band_press(&mut self) {
        self.band_release_swallow = false;
        self.band_press_target = None;
        self.band_press_button = None;
        self.band_extra_press = None;
    }

    /// Assigns the next monotonic admission sequence, failing closed at
    /// `u64::MAX` (CTX-1332, C-D4).
    fn next_admission_seq(&mut self) -> Option<u64> {
        let next = self.admission_seq.checked_add(1)?;
        self.admission_seq = next;
        Some(next)
    }

    /// Returns the last presented origin for a painted band, if any
    /// (CTX-1332, C-D4/C-D5).
    ///
    /// The origin must match the live band identity *and* the live geometry —
    /// row and column, from [`Self::band_paint_rect`] — so a replacement mount,
    /// version reset, edge move, or resize (on either axis, issue #3171)
    /// refuses the gesture instead of retargeting it.
    fn published_band_origin(&self, edge: BandEdge, visible_index: usize) -> Option<&BandOrigin> {
        let index = u16::try_from(visible_index).ok()?;
        let (row, column, _, _) = self.band_paint_rect(edge, visible_index)?;
        let bands = self.visible_edge_bands(edge);
        let band = bands.get(visible_index)?;
        self.published_band_origins.iter().find(|origin| {
            origin.edge == edge
                && origin.visible_index == index
                && origin.row == row
                && origin.column == column
                && origin.plugin_id == band.plugin_id
                && origin.slot == band.slot
                && origin.version == band.version
        })
    }

    /// Whether `origin` is still the last presented origin for its live band
    /// (CTX-1332, C-D4/C-D5).
    ///
    /// `false` when the origin was never published, the band moved, a
    /// replacement mount restarted the version, or the geometry changed.
    #[doc(hidden)]
    #[must_use]
    pub fn band_origin_is_current(&self, origin: &BandOrigin) -> bool {
        // The origin must still be the live capture for this band (pending
        // origins follow the live mount projection) *and* the last published
        // executable origin. A remount or edge move invalidates immediately,
        // even before the next frame commits.
        if !self
            .pending_band_origins
            .iter()
            .any(|pending| pending == origin)
        {
            return false;
        }
        self.published_band_origin(origin.edge, usize::from(origin.visible_index))
            .is_some_and(|current| current == origin)
    }

    /// Origins published by the last successfully presented frame
    /// (CTX-1332, C-D4). Read-only diagnostic/test accessor.
    #[doc(hidden)]
    #[must_use]
    pub fn published_band_origins(&self) -> &[BandOrigin] {
        &self.published_band_origins
    }

    /// Damage identity of every mounted band, including edges and executable
    /// origins (CTX-1332, C-D4/F7).
    ///
    /// Painted bands carry their pending [`BandOrigin`] on either axis (issue
    /// #3171: a column band's origin carries its column geometry); hidden bands
    /// carry `None` (they are never clickable) but still participate in damage
    /// identity through plugin id, slot, version, and edge.
    #[doc(hidden)]
    #[must_use]
    pub fn band_frame_identity(&self) -> Vec<BandFrameIdentity> {
        let mut identities = Vec::new();
        let mut used = BTreeSet::new();
        for edge in [
            BandEdge::Top,
            BandEdge::Bottom,
            BandEdge::Left,
            BandEdge::Right,
        ] {
            for band in self.chrome_bands.edge(edge) {
                let origin = if band_is_visible(&band.root) {
                    self.pending_band_origins
                        .iter()
                        .enumerate()
                        .find(|(index, origin)| {
                            !used.contains(index)
                                && origin.edge == edge
                                && origin.plugin_id == band.plugin_id
                                && origin.slot == band.slot
                                && origin.version == band.version
                        })
                        .map(|(index, origin)| {
                            used.insert(index);
                            origin.clone()
                        })
                } else {
                    None
                };
                identities.push(BandFrameIdentity {
                    plugin_id: band.plugin_id.clone(),
                    slot: band.slot,
                    version: band.version,
                    edge,
                    origin,
                });
            }
        }
        identities
    }

    /// Selects at most `max` queued admissions, leaving the unselected tail
    /// queued (CTX-1332, C-D9/C-D10).
    ///
    /// This replaces a `mem::take` drain for the cold pump: a partial batch
    /// never silently drops work. Order preserves the existing click-then-
    /// scroll queue order (cross-owner chronological ordering remains a
    /// separate proposal, C-D7).
    #[doc(hidden)]
    pub fn drain_band_admissions(&mut self, max: usize) -> Vec<BandAdmission> {
        let mut admissions = Vec::with_capacity(
            max.min(
                self.band_click_queue
                    .len()
                    .saturating_add(self.band_scroll_queue.len()),
            ),
        );
        let click_take = max.min(self.band_click_queue.len());
        for request in self.band_click_queue.drain(..click_take) {
            admissions.push(BandAdmission::Click(request));
        }
        let scroll_take = max
            .saturating_sub(admissions.len())
            .min(self.band_scroll_queue.len());
        for request in self.band_scroll_queue.drain(..scroll_take) {
            admissions.push(BandAdmission::Scroll(request));
        }
        admissions
    }

    /// Sets chrome bands together with the live mount projection used to
    /// capture origins (CTX-1332, C-D4).
    ///
    /// Additive `#[doc(hidden)]` entry: `set_chrome_bands` and
    /// `ChromeBands::from_mounts` keep their existing public shapes.
    #[doc(hidden)]
    pub fn set_chrome_bands_with_mounts(&mut self, bands: ChromeBands, mounts: Vec<BandMount>) {
        if self.chrome_bands != bands {
            self.pending_full_redraw = true;
        }
        self.chrome_bands = bands;
        self.rebuild_pending_band_origins(mounts);
        // Issue #2411: same rule as `set_chrome_bands` — a replaced tree ends
        // a hover whose region it no longer carries.
        self.retain_live_band_hover();
    }

    /// Recomputes pending origins from the current bands and live mounts,
    /// then eagerly cancels queued/armed admissions whose origin is no longer
    /// live (CTX-1332, C-D4/C-D5).
    fn rebuild_pending_band_origins(&mut self, mounts: Vec<BandMount>) {
        let mut by_key: BTreeMap<(String, UiSlot, u32), VecDeque<(u32, i64)>> = BTreeMap::new();
        for mount in mounts {
            by_key
                .entry((mount.plugin_id, mount.slot, mount.version))
                .or_default()
                .push_back((mount.generation, mount.handle));
        }

        let mut pending = Vec::new();
        for edge in [
            BandEdge::Top,
            BandEdge::Bottom,
            BandEdge::Left,
            BandEdge::Right,
        ] {
            let bands: Vec<(String, UiSlot, u32, bool)> = self
                .chrome_bands
                .edge(edge)
                .iter()
                .map(|band| {
                    (
                        band.plugin_id.clone(),
                        band.slot,
                        band.version,
                        band_is_visible(&band.root),
                    )
                })
                .collect();
            let mut visible_index = 0u16;
            for (plugin_id, slot, version, visible) in bands {
                let mount = by_key
                    .get_mut(&(plugin_id.clone(), slot, version))
                    .and_then(VecDeque::pop_front);
                if !visible {
                    continue;
                }
                if let Some((generation, handle)) = mount {
                    // C-D4 counter/version exhaustion fails closed: never
                    // issue an origin whose version can no longer advance.
                    if version != u32::MAX && generation != 0 {
                        // Issue #3171: both axes publish an origin through the
                        // one granted rect, carrying the band's own geometry
                        // (row for a row band, column for a column band) so a
                        // reflow on either axis invalidates it.
                        if let Some((row, column, _, _)) =
                            self.band_paint_rect(edge, usize::from(visible_index))
                        {
                            pending.push(BandOrigin {
                                plugin_id,
                                generation,
                                handle,
                                slot,
                                version,
                                edge,
                                visible_index,
                                row,
                                column,
                            });
                        }
                    }
                }
                visible_index = visible_index.saturating_add(1);
            }
        }
        self.pending_band_origins = pending;
        let live = self.pending_band_origins.clone();
        self.retain_live_band_admissions(&live);
    }

    /// Eagerly cancels queued and armed admissions whose origin is not in the
    /// live set (CTX-1332, C-D5).
    ///
    /// Deliberately does **not** call [`Self::clear_band_press`]: physical
    /// release swallowing stays owned by the existing press state, so an
    /// invalidated action still consumes its release.
    pub(super) fn retain_live_band_admissions(&mut self, live: &[BandOrigin]) {
        let live_origin = |origin: &Option<BandOrigin>| {
            origin.as_ref().is_some_and(|origin| live.contains(origin))
        };
        self.band_click_queue
            .retain(|request| live_origin(&request.origin));
        self.band_scroll_queue
            .retain(|request| live_origin(&request.origin));
        if let Some(PressedBandTarget::Hit { request, .. }) = &self.band_press_target {
            if !live_origin(&request.origin) {
                // Cancel the armed target but keep the release swallow.
                self.band_press_target = None;
            }
        }
    }

    /// Resolves the release paired with a band-consumed press (CTX-0946 C1).
    ///
    /// Returns `true` (consume the release) when the swallow is armed.
    /// Standard button semantics: the
    /// release routes only when it resolves to the same band run the press
    /// armed (band, run, and resolved request all equal, so content that
    /// changed mid-gesture cannot misroute). A drag onto another span or
    /// band counts [`BandHostStats::clicks_unclaimed`]; a release denied on
    /// the same denied target the press armed counts
    /// [`BandHostStats::clicks_denied`]. A full queue drops fail-closed and
    /// counts [`BandHostStats::queue_drops`].
    pub(super) fn band_release(&mut self, mouse_button: MouseButton) -> bool {
        let Some(button) = BandButton::from_mouse(mouse_button) else {
            return false;
        };
        // A second button pressed on the same band row while the first was
        // armed: consume its release without resolving the main gesture.
        if self.band_extra_press == Some(button) {
            self.band_extra_press = None;
            return true;
        }
        if !self.band_release_swallow {
            return false;
        }
        // A release of another button never resolves the armed gesture; it
        // is still chrome and must not fall through to clipboard/capture.
        if self.band_press_button != Some(button) {
            return true;
        }
        self.band_release_swallow = false;
        self.band_press_button = None;
        let pressed = self.band_press_target.take();
        let Some(pos) = self.last_cursor else {
            self.band_stats.clicks_unclaimed = self.band_stats.clicks_unclaimed.saturating_add(1);
            return true;
        };
        let Some(cell) = self.plugin_band_cell_hit(pos) else {
            self.band_stats.clicks_unclaimed = self.band_stats.clicks_unclaimed.saturating_add(1);
            return true;
        };
        let released = self.band_target_at(&cell, button);
        match (pressed, released) {
            (
                Some(PressedBandTarget::Hit {
                    edge,
                    visible_index,
                    key,
                    button,
                    request,
                }),
                PressedBandTarget::Hit {
                    edge: release_edge,
                    visible_index: release_visible,
                    key: release_key,
                    button: release_button,
                    request: release_request,
                },
            ) if edge == release_edge
                && visible_index == release_visible
                && key == release_key
                && button == release_button
                && request == release_request =>
            {
                let mut request = *request;
                // CTX-1332 C-D4/C-D5: the press-captured origin must still be
                // the live published origin and the live band identity.
                if let Some(origin) = request.origin.clone() {
                    if !self.band_origin_is_current(&origin) {
                        self.band_stats.clicks_denied =
                            self.band_stats.clicks_denied.saturating_add(1);
                        return true;
                    }
                }
                // C-D4 exhaustion fails closed: never wrap the sequence.
                let Some(seq) = self.next_admission_seq() else {
                    self.band_stats.clicks_denied = self.band_stats.clicks_denied.saturating_add(1);
                    return true;
                };
                request.admission_seq = seq;
                if self.band_click_queue.len() >= BAND_CLICK_QUEUE_MAX {
                    self.band_stats.queue_drops = self.band_stats.queue_drops.saturating_add(1);
                } else {
                    self.band_click_queue.push(request);
                    self.band_stats.clicks_routed = self.band_stats.clicks_routed.saturating_add(1);
                }
            }
            (Some(PressedBandTarget::Denied), PressedBandTarget::Denied) => {
                self.band_stats.clicks_denied = self.band_stats.clicks_denied.saturating_add(1);
            }
            _ => {
                self.band_stats.clicks_unclaimed =
                    self.band_stats.clicks_unclaimed.saturating_add(1);
            }
        }
        true
    }

    /// Drains queued band click requests for application dispatch (CTX-0946 C1).
    ///
    /// The application dispatches each through
    /// `PluginRuntime::dispatch_command`; the runtime never touches a VM.
    /// Order is press order; the queue is empty in the common case.
    pub fn drain_band_clicks(&mut self) -> Vec<BandClickRequest> {
        std::mem::take(&mut self.band_click_queue)
    }

    /// Routes a wheel tick over a plugin band row into the scroll drain
    /// queue (CTX-1190, issue #2008).
    ///
    /// Returns `true` (consume the tick: no viewport scroll, no mouse
    /// report, no alt-scroll keys) when the last-known cursor sits on a
    /// painted band row whose resolved span carries `on_scroll`. The queued
    /// request carries the host-computed `delta` (signed lines, positive for
    /// wheel-up); consecutive ticks for one target coalesce into a single
    /// slot with saturating math, so a burst costs one dispatch.
    ///
    /// Returns `false` (fall through to the viewport/capture/alt paths
    /// unchanged) for a zero step, an unknown cursor, an off-band cursor,
    /// or a row/span without a routable scroll binding — unbound nodes keep
    /// today's behavior with no regression. Denied and unclaimed wheels are
    /// counted in [`BandHostStats`] either way.
    pub(super) fn band_scroll(&mut self, delta: i32) -> bool {
        if delta == 0 {
            return false;
        }
        let outcome = self.scroll_target_at_cursor();
        match outcome {
            ScrollTargetOutcome::Hit(target) => {
                // CTX-1332 C-D4/C-D5: the captured origin must still be the
                // live published origin and the live band identity.
                if let Some(origin) = target.origin.clone() {
                    if !self.band_origin_is_current(&origin) {
                        self.band_stats.scrolls_denied =
                            self.band_stats.scrolls_denied.saturating_add(1);
                        return true;
                    }
                }
                // C-D4 exhaustion fails closed: never wrap the sequence.
                let Some(seq) = self.next_admission_seq() else {
                    self.band_stats.scrolls_denied =
                        self.band_stats.scrolls_denied.saturating_add(1);
                    return true;
                };
                self.queue_band_scroll(BandScrollRequest {
                    plugin_id: target.plugin_id,
                    command: target.command,
                    args: target.args,
                    region: target.region,
                    delta,
                    origin: target.origin,
                    admission_seq: seq,
                });
                true
            }
            ScrollTargetOutcome::Denied => {
                self.band_stats.scrolls_denied = self.band_stats.scrolls_denied.saturating_add(1);
                false
            }
            ScrollTargetOutcome::Overflow => {
                self.band_stats.region_overflows =
                    self.band_stats.region_overflows.saturating_add(1);
                self.band_stats.scrolls_denied = self.band_stats.scrolls_denied.saturating_add(1);
                // Consume the tick as chrome: an over-cap band refuses the
                // action but must not leak the wheel to the viewport behind.
                true
            }
            ScrollTargetOutcome::Unclaimed => {
                self.band_stats.scrolls_unclaimed =
                    self.band_stats.scrolls_unclaimed.saturating_add(1);
                false
            }
            ScrollTargetOutcome::OffBand => false,
        }
    }

    /// Resolves the cursor's band scroll target without mutating state.
    ///
    /// Pure classification behind [`Self::band_scroll`]: off-band (fall
    /// through silently), unclaimed (fall through, counted), denied (fall
    /// through, counted), or a resolved hit.
    fn scroll_target_at_cursor(&self) -> ScrollTargetOutcome {
        let Some(pos) = self.last_cursor else {
            return ScrollTargetOutcome::OffBand;
        };
        let Some(cell) = self.plugin_band_cell_hit(pos) else {
            return ScrollTargetOutcome::OffBand;
        };
        // Issue #3171: the same axis-general line resolution click and hover
        // use, so a column band's wheel tick reads its own content line.
        let Some(line) = self.band_line_at(&cell) else {
            return ScrollTargetOutcome::Unclaimed;
        };
        let owner = line.plugin_id;
        // Issue #2248: a declared region owns its rect; an over-cap band
        // refuses the wheel (fail-closed) instead of routing a partial table.
        let regions = match self.band_item_regions(line.edge, line.visible_index) {
            Ok(regions) => regions,
            Err(BandRegionError::MissingBand) => return ScrollTargetOutcome::Unclaimed,
            Err(BandRegionError::Overflow { .. }) => return ScrollTargetOutcome::Overflow,
        };
        let declared =
            resolve_item_region(&regions, line.row, line.col).map(|item| item.region.clone());
        let outcome = match declared.as_ref() {
            Some(region) => resolve_scroll_for_region(&owner, &line.runs, region, line.col),
            None => resolve_scroll_in_band(&owner, &line.runs, line.col),
        };
        match outcome {
            BandScrollOutcome::Hit(mut target) => {
                target.origin = self
                    .published_band_origin(line.edge, line.visible_index)
                    .cloned();
                ScrollTargetOutcome::Hit(target)
            }
            BandScrollOutcome::Denied => ScrollTargetOutcome::Denied,
            BandScrollOutcome::NoClaim => ScrollTargetOutcome::Unclaimed,
        }
    }

    /// Queues one resolved band scroll for application dispatch (CTX-1190).
    ///
    /// Consecutive requests for one target (same plugin, verb, and declared
    /// args) coalesce: the deltas sum with saturating math into the trailing
    /// slot and count [`BandHostStats::scrolls_coalesced`], so a 100-tick
    /// frame drains as one dispatch with a bounded step — never a VM-budget
    /// overrun. A full queue drops the newest request fail-closed and counts
    /// [`BandHostStats::scroll_queue_drops`].
    pub(super) fn queue_band_scroll(&mut self, request: BandScrollRequest) {
        if request.delta == 0 {
            return;
        }
        if let Some(back) = self.band_scroll_queue.last_mut() {
            if back.plugin_id == request.plugin_id
                && back.command == request.command
                && back.args == request.args
                && back.region == request.region
                && back.origin == request.origin
            {
                back.delta = back.delta.saturating_add(request.delta);
                self.band_stats.scrolls_coalesced =
                    self.band_stats.scrolls_coalesced.saturating_add(1);
                return;
            }
        }
        if self.band_scroll_queue.len() >= BAND_SCROLL_QUEUE_MAX {
            self.band_stats.scroll_queue_drops =
                self.band_stats.scroll_queue_drops.saturating_add(1);
        } else {
            self.band_scroll_queue.push(request);
            self.band_stats.scrolls_routed = self.band_stats.scrolls_routed.saturating_add(1);
        }
    }

    /// Drains queued band scroll requests for application dispatch (CTX-1190).
    ///
    /// The application dispatches each through
    /// `PluginRuntime::dispatch_command` with the declared args plus the
    /// host-computed `delta` (see [`band_scroll_args_table`]); the runtime
    /// never touches a VM. Order is tick order modulo coalescing; the queue
    /// is empty in the common case.
    pub fn drain_band_scrolls(&mut self) -> Vec<BandScrollRequest> {
        std::mem::take(&mut self.band_scroll_queue)
    }

    /// Updates the hover state from a pointer position (issue #2411).
    ///
    /// One call per pointer motion. The host resolves the hover binding under
    /// the pointer from the same width-aware runs paint and click hit-testing
    /// use, and queues at most one transition per changed region: leaving the
    /// previous item (when there was one) and entering the new one. Re-entering
    /// the same region is a no-op, so steady hover costs one bounded hit test
    /// and no dispatch.
    ///
    /// Hover is presentation-only: it never captures input, never sets
    /// `modal_active`, and carries a stable region id plus a phase. A binding
    /// with an unroutable command counts [`BandHostStats::hovers_denied`] and
    /// is treated as no target (a leave for the previous item still fires).
    ///
    /// Motion is inert while the window is unfocused or while a focusable
    /// overlay capture holds: a modal must never re-arm a tooltip underneath
    /// itself. The teardown transition (one leave) is queued by
    /// `Self::clear_band_hover` when focus or capture state changes, not by
    /// motion.
    pub fn update_band_hover(&mut self, pos: CursorPosition) {
        if !self.focused || self.hover_suppressed_by_overlay {
            return;
        }
        let next = match self.band_hover_probe(pos) {
            HoverProbe::Target(state) => Some(state),
            HoverProbe::Denied => {
                self.band_stats.hovers_denied = self.band_stats.hovers_denied.saturating_add(1);
                None
            }
            HoverProbe::None => None,
        };
        let current = self.band_hover.clone();
        match (&current, &next) {
            (Some(current), Some(next)) if current.same_item(next) => return,
            (None, None) => return,
            _ => {}
        }
        if let Some(current) = current {
            self.queue_band_hover(current.leave_request());
        }
        match next {
            Some(next) => {
                self.queue_band_hover(next.enter_request());
                self.band_hover = Some(next);
            }
            None => self.band_hover = None,
        }
    }

    /// Resolves the hoverable region under `pos` without mutating state
    /// (issue #2411, issue #3171).
    fn band_hover_probe(&self, pos: CursorPosition) -> HoverProbe {
        let Some(cell) = self.plugin_band_cell_hit(pos) else {
            return HoverProbe::None;
        };
        // Issue #3171: hover resolves on the band's own content line, the same
        // mapping click and scroll use, so a column band hovers its items.
        let Some(line) = self.band_line_at(&cell) else {
            return HoverProbe::None;
        };
        let owner = line.plugin_id;
        match resolve_hover_in_band(&owner, &line.runs, line.col) {
            BandHoverOutcome::Target(target) => HoverProbe::Target(BandHoverState {
                edge: line.edge,
                visible_index: line.visible_index,
                plugin_id: target.plugin_id,
                region: target.region,
                command: target.command,
                args: target.args,
                col: line.col,
                origin: self
                    .published_band_origin(line.edge, line.visible_index)
                    .cloned(),
                announced: false,
            }),
            BandHoverOutcome::Denied => HoverProbe::Denied,
            BandHoverOutcome::NoClaim => HoverProbe::None,
        }
    }

    /// Queues one resolved hover transition (issue #2411).
    ///
    /// At most one dispatch per region per drain: a newer phase **or a newer
    /// presented origin** for a region already queued replaces that entry (a
    /// reversed transition collapses to its final phase, and a revalidated
    /// enter carries the current frame's origin instead of the stale one it
    /// was first queued with). Only a transition identical in phase, origin,
    /// plugin, and region coalesces. A full queue drops the newest transition
    /// fail-closed and counts [`BandHostStats::hover_queue_drops`].
    ///
    /// `enter` is fenced by the presented origin: a remount, edge move,
    /// reflow, or version reset never retargets hover to different content,
    /// and an enter whose band is newer than the last presented frame is
    /// deferred ([`BandHostStats::hovers_deferred`]) until
    /// [`Self::revalidate_band_hover`] can re-derive it. `leave` is **never**
    /// fenced: it hides/cleans up and grants no authority, so a version bump
    /// or a removed band must still deliver exactly one leave — refusing it
    /// would strand the plugin's tooltip state.
    fn queue_band_hover(&mut self, request: BandHoverRequest) {
        if request.phase == BandHoverPhase::Enter {
            let Some(origin) = request.origin.as_ref() else {
                self.band_stats.hovers_deferred = self.band_stats.hovers_deferred.saturating_add(1);
                return;
            };
            if !self.band_origin_is_current(origin) {
                self.band_stats.hovers_denied = self.band_stats.hovers_denied.saturating_add(1);
                return;
            }
        }
        let same_region = |queued: &BandHoverRequest| {
            queued.plugin_id == request.plugin_id && queued.region == request.region
        };
        if let Some(index) = self.band_hover_queue.iter().position(same_region) {
            // A newer phase replaces the queued one, and so does a newer
            // presented origin for the same phase: a band version bump can
            // re-derive an enter that was queued against the previous frame,
            // and the old origin would be refused at dispatch (issue #2411
            // third review). Only a byte-identical transition coalesces.
            let queued = &self.band_hover_queue[index];
            if queued.phase != request.phase || queued.origin != request.origin {
                self.band_hover_queue[index] = request;
            }
            self.band_stats.hovers_coalesced = self.band_stats.hovers_coalesced.saturating_add(1);
            return;
        }
        if self.band_hover_queue.len() >= BAND_HOVER_QUEUE_MAX {
            self.band_stats.hover_queue_drops = self.band_stats.hover_queue_drops.saturating_add(1);
            return;
        }
        self.band_hover_queue.push_back(request);
        self.band_stats.hovers_routed = self.band_stats.hovers_routed.saturating_add(1);
    }

    /// Drains queued band hover transitions for application dispatch
    /// (issue #2411).
    ///
    /// The application dispatches an `enter` through
    /// `PluginRuntime::dispatch_ui_command` with the captured origin (the same
    /// presented-frame fence clicks and scrolls use), and a `leave` through
    /// the unfenced command path (it only hides/cleans up). Registration,
    /// capability, and args-schema gates apply to both; the runtime never
    /// touches a VM. Order is transition order modulo per-region coalescing;
    /// the queue is empty in the common case.
    pub fn drain_band_hovers(&mut self) -> Vec<BandHoverRequest> {
        self.band_hover_queue.drain(..).collect()
    }

    /// Re-derives a deferred or stale hover after a frame published new band
    /// origins (issue #2411).
    ///
    /// Runs once per cold phase before the hover queue drains. When the
    /// pointer still sits on the same item and the live band now has a
    /// presented origin, the state picks up that origin; an `enter` that was
    /// deferred because the band was newer than the last presented frame is
    /// queued here, so a tooltip appears as soon as its band is presented
    /// (without waiting for another pointer motion). A delivered enter is
    /// never re-fired by a content/version bump.
    pub fn revalidate_band_hover(&mut self) {
        let Some(current) = self.band_hover.clone() else {
            return;
        };
        let stale = current
            .origin
            .as_ref()
            .is_none_or(|origin| !self.band_origin_is_current(origin));
        if !stale {
            return;
        }
        let Some(pos) = self.last_cursor else {
            return;
        };
        let HoverProbe::Target(next) = self.band_hover_probe(pos) else {
            return;
        };
        if !next.same_item(&current) {
            return;
        }
        let Some(origin) = next.origin.clone() else {
            // The band is still newer than the last presented frame: keep the
            // unannounced state and retry on the next cold phase.
            return;
        };
        let refreshed = BandHoverState {
            origin: Some(origin),
            announced: current.announced,
            ..next
        };
        self.band_hover = Some(refreshed.clone());
        if !current.announced {
            self.queue_band_hover(refreshed.enter_request());
        }
    }

    /// Forgets the live hover without queueing a leave (issue #2411).
    ///
    /// Used when an `enter` dispatch was refused outright (for example a
    /// stale origin caught between the queue fence and the VM borrow): the
    /// plugin never saw the enter, so no leave is owed, and the next pointer
    /// motion starts a fresh hover instead of retrying every cold phase.
    pub fn abandon_band_hover(&mut self) {
        self.band_hover = None;
    }

    /// Records that an `enter` transition reached the plugin runtime
    /// (issue #2411).
    ///
    /// The application calls this after dispatching (or accepting a parked)
    /// enter. It stops [`Self::revalidate_band_hover`] from re-firing the
    /// enter on later content/version bumps and lets the overlay host anchor
    /// the tooltip for this item.
    pub fn confirm_band_hover(&mut self, region: &UiRegion) {
        if let Some(state) = self.band_hover.as_mut() {
            if state.region == *region {
                state.announced = true;
            }
        }
    }

    /// The live hovered band region rect, if any (issue #2411).
    ///
    /// Derived from the current tree, so an overlay host can anchor a tooltip
    /// to the region Core is actually painting rather than to a stale
    /// rectangle. `None` when nothing is hovered, when the region no longer
    /// exists in the live band, or when the item's `enter` has not reached the
    /// plugin yet (a deferred transition must not surface stale tooltip
    /// content at a new anchor).
    #[must_use]
    pub fn hovered_band_region(&self) -> Option<BandItemRegion> {
        let state = self.band_hover.as_ref()?;
        if !state.announced {
            return None;
        }
        self.hover_region_rect(state)
    }

    /// Live rectangle of the hovered item, derived from the hover bindings
    /// (issue #2411).
    ///
    /// Hover identity is declared on `on_hover`, which may differ from a
    /// span's click/scroll identity, so this walks the hover bindings instead
    /// of the click-region table. Adjacent runs that share the hover region
    /// merge into one rect, exactly like the click table merges its items.
    fn hover_region_rect(&self, state: &BandHoverState) -> Option<BandItemRegion> {
        let row = self.visible_band_row(state.edge, state.visible_index)?;
        let bands = self.visible_edge_bands(state.edge);
        let band = bands.get(state.visible_index)?;
        if band.plugin_id != state.plugin_id {
            return None;
        }
        let width = usize::from(self.window_cells().width);
        let (_, runs) = flatten_band_runs_in_width(&band.root, width);
        let matches = |run: &BandRun| {
            run.on_hover
                .as_ref()
                .and_then(|binding| binding.region.as_ref())
                == Some(&state.region)
        };
        // The entered column must still hold the same hover region: a
        // replacement tree, a reflow, or a moved item ends the hover instead
        // of anchoring a tooltip to different content.
        let covering = runs.iter().position(|run| {
            state.col >= run.start_col && state.col < run.end_col && matches(run)
        })?;
        let mut start = runs[covering].start_col;
        let mut end = runs[covering].end_col;
        let mut index = covering;
        while index > 0 {
            let previous = &runs[index - 1];
            if previous.end_col == start && matches(previous) {
                start = previous.start_col;
                index -= 1;
            } else {
                break;
            }
        }
        index = covering;
        while index + 1 < runs.len() {
            let next = &runs[index + 1];
            if next.start_col == end && matches(next) {
                end = next.end_col;
                index += 1;
            } else {
                break;
            }
        }
        let start = start.min(width);
        let end = end.min(width);
        if start >= end {
            return None;
        }
        Some(BandItemRegion {
            plugin_id: band.plugin_id.clone(),
            edge: state.edge,
            visible_index: state.visible_index,
            row,
            region: state.region.clone(),
            start_col: start,
            end_col: end,
            version: band.version,
        })
    }

    /// Queues a leave for the current hover and forgets it
    /// (issue #2411).
    ///
    /// Used when hover ownership ends without a pointer transition: window
    /// focus loss, or a band replacement that removed the hovered region.
    /// The leave still dispatches once (a plugin can hide its tooltip), and
    /// no capture or modal state is touched.
    pub(super) fn clear_band_hover(&mut self) {
        if let Some(state) = self.band_hover.take() {
            self.queue_band_hover(state.leave_request());
        }
    }

    /// Ends hover ownership when a focusable overlay capture starts
    /// (issue #2411).
    ///
    /// Drops every pending transition first, then queues exactly one `leave`
    /// for the item that was live. The application does not enter a plugin VM
    /// for hover while capture holds, so the queued leave dispatches when the
    /// modal ends — and a tooltip can neither stay visible nor re-arm under
    /// the modal, because Core stops selecting the hover surface as soon as
    /// capture is active.
    pub(super) fn clear_band_hover_for_capture(&mut self) {
        self.band_hover_queue.clear();
        self.clear_band_hover();
    }

    /// Clears the hover when the live band no longer carries its region
    /// (issue #2411).
    ///
    /// Called after a band replacement: the state names a band index and a
    /// declared region, so a remount, an edge move, or an item disappearing
    /// ends the hover instead of leaving a tooltip anchored to stale
    /// geometry.
    pub(super) fn retain_live_band_hover(&mut self) {
        let Some(state) = self.band_hover.as_ref() else {
            return;
        };
        let region_live = self
            .hover_region_rect(state)
            .is_some_and(|rect| rect.plugin_id == state.plugin_id);
        if !region_live {
            self.clear_band_hover();
        }
    }

    /// Whether a band-consumed press still owns its release.
    #[must_use]
    pub fn band_release_armed(&self) -> bool {
        self.band_release_swallow
    }

    /// Version snapshot of every mounted band, in stacking order, including
    /// the band edge (CTX-0946 C2, CTX-1332 F7).
    ///
    /// The present path compares a richer origin+geometry identity
    /// ([`Self::band_frame_identity`]); this tuple form remains for callers
    /// that need the plain version snapshot. The edge is part of the tuple so
    /// an edge move at an unchanged version does not compare equal.
    #[must_use]
    pub fn chrome_band_versions(&self) -> Vec<(String, UiSlot, u32, BandEdge)> {
        let mut versions = Vec::new();
        for edge in [
            BandEdge::Top,
            BandEdge::Bottom,
            BandEdge::Left,
            BandEdge::Right,
        ] {
            for band in self.chrome_bands.edge(edge) {
                versions.push((band.plugin_id.clone(), band.slot, band.version, edge));
            }
        }
        versions
    }

    /// Headless host statistics for band routing and paint.
    #[must_use]
    pub fn band_host_stats(&self) -> BandHostStats {
        self.band_stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One band run with the given declared `fg` and hover-region identity.
    fn band_run(fg: Option<&str>, region: Option<&str>) -> BandRun {
        BandRun {
            start_col: 0,
            end_col: 5,
            start_char: 0,
            end_char: 5,
            fg: fg.map(str::to_string),
            bg: None,
            bold: false,
            on_click: None,
            on_scroll: None,
            on_hover: region.map(|id| ClickCommand {
                command: String::from("test-bar:detail"),
                args: Vec::new(),
                region: Some(UiRegion {
                    id: id.to_string(),
                    kind: bitty_lua::ui::UiRegionKind::Module,
                }),
            }),
            region: None,
        }
    }

    #[test]
    fn default_band_run_fg_token_keys_off_the_hover_region_identity() {
        // bitty#3108 round-2 (CTX-1788): the default fg is chosen from the
        // module's `bar:module:<name>` hover-region identity, never from
        // span text, and never overrides a declared fg/bg.
        assert_eq!(
            default_band_run_fg_token(&band_run(None, Some("bar:module:clock"))),
            Some("accent")
        );
        // Only the first segment after the prefix names the module; a
        // per-item suffix (`cpu:0`) is ignored.
        assert_eq!(
            default_band_run_fg_token(&band_run(None, Some("bar:module:cpu:0"))),
            Some("green")
        );
        // Unknown module, a non-bar region id, and no hover binding all fall
        // back to the span default instead of guessing.
        assert_eq!(
            default_band_run_fg_token(&band_run(None, Some("bar:module:unknown"))),
            None
        );
        assert_eq!(
            default_band_run_fg_token(&band_run(None, Some("clock"))),
            None
        );
        assert_eq!(default_band_run_fg_token(&band_run(None, None)), None);
        // A declared fg or bg opts out entirely, even for a known module.
        assert_eq!(
            default_band_run_fg_token(&band_run(Some("red"), Some("bar:module:clock"))),
            None
        );

        // Every token the table returns is a real palette role: a typo here
        // would silently paint the span default instead of the module colour.
        let palette = ThemePalette::bitty_dark();
        for module in [
            "clock",
            "network",
            "cpu",
            "memory",
            "battery",
            "pulseaudio",
            "microphone",
            "bluetooth",
            "backlight",
            "idle",
            "spectrum",
        ] {
            let id = format!("bar:module:{module}");
            let token = default_band_run_fg_token(&band_run(None, Some(&id)))
                .unwrap_or_else(|| panic!("{module} must map to a token"));
            assert!(
                resolve_band_token(token, &palette).is_some(),
                "{module} -> {token} must resolve against the default palette"
            );
        }
    }

    fn text_node(text: &str) -> UiNode {
        UiNode::Text {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
            anchor: None,
            on_hover: None,
        }
    }

    fn click_node(text: &str, command: &str) -> UiNode {
        UiNode::Text {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: Some(ClickCommand {
                command: command.to_string(),
                args: vec![("id".to_string(), ClickArg::Number(3.0))],
                region: None,
            }),
            on_scroll: None,
            anchor: None,
            on_hover: None,
        }
    }

    fn scroll_node(text: &str, command: &str) -> UiNode {
        UiNode::Text {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: Some(ClickCommand {
                command: command.to_string(),
                args: vec![("step".to_string(), ClickArg::Number(2.0))],
                region: None,
            }),
            anchor: None,
            on_hover: None,
        }
    }

    #[test]
    fn flatten_assigns_char_columns_in_walk_order() {
        let root = UiNode::row(vec![text_node("ab"), text_node("c")]);
        let (text, runs) = flatten_band_runs(&root);
        assert_eq!(text, "abc");
        assert_eq!(
            runs.iter()
                .map(|run| (run.start_col, run.end_col))
                .collect::<Vec<_>>(),
            vec![(0, 2), (2, 3)]
        );
    }

    #[test]
    fn flatten_inherits_container_style_and_click() {
        let click = ClickCommand {
            command: "owner:go".to_string(),
            args: Vec::new(),
            region: None,
        };
        let root = UiNode::Row {
            align: None,
            children: vec![text_node("x")],
            fg: Some("accent".to_string()),
            bg: None,
            bold: Some(true),
            on_click: Some(click.clone()),
            on_scroll: None,
            anchor: None,
            on_hover: None,
        };
        let (_, runs) = flatten_band_runs(&root);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].fg.as_deref(), Some("accent"));
        assert!(runs[0].bold);
        assert_eq!(runs[0].on_click, Some(click));
    }

    #[test]
    fn leaf_style_beats_container_style() {
        let root = UiNode::Row {
            align: None,
            children: vec![UiNode::Text {
                text: "x".to_string(),
                fg: Some("foreground".to_string()),
                bg: None,
                bold: Some(false),
                on_click: None,
                on_scroll: None,
                anchor: None,
                on_hover: None,
            }],
            fg: Some("accent".to_string()),
            bg: None,
            bold: Some(true),
            on_click: None,
            on_scroll: None,
            anchor: None,
            on_hover: None,
        };
        let (_, runs) = flatten_band_runs(&root);
        assert_eq!(runs[0].fg.as_deref(), Some("foreground"));
        assert!(!runs[0].bold);
    }

    #[test]
    fn flatten_carries_scroll_with_nearest_wins() {
        // CTX-1190 (#2008): on_scroll inherits exactly like on_click —
        // container bindings cover bare descendants, leaf bindings win.
        let scroll = ClickCommand {
            command: "owner:scroll".to_string(),
            args: Vec::new(),
            region: None,
        };
        let root = UiNode::Row {
            align: None,
            children: vec![text_node("x"), scroll_node("y", "owner:leaf")],
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: Some(scroll.clone()),
            anchor: None,
            on_hover: None,
        };
        let (_, runs) = flatten_band_runs(&root);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].on_scroll, Some(scroll));
        assert_eq!(
            runs[1].on_scroll.as_ref().expect("leaf scroll").command,
            "owner:leaf"
        );
        // Click bindings are untouched by the scroll inheritance.
        assert_eq!(runs[0].on_click, None);
        assert_eq!(runs[1].on_click, None);
    }

    #[test]
    fn resolve_scroll_hit_no_claim_denied() {
        // CTX-1190 (#2008): scroll resolution mirrors click resolution —
        // bound spans hit, bare spans claim nothing, foreign qualifiers deny.
        let (_, runs) = flatten_band_runs(&UiNode::row(vec![
            text_node("ab"),
            scroll_node("cd", "test-bar:scroll"),
            UiNode::Text {
                text: "ef".to_string(),
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
                on_scroll: Some(ClickCommand {
                    command: "other-plugin:scroll".to_string(),
                    args: Vec::new(),
                    region: None,
                }),
                anchor: None,
                on_hover: None,
            },
        ]));
        assert!(matches!(
            resolve_scroll_in_band("test-bar", &runs, 0),
            BandScrollOutcome::NoClaim
        ));
        match resolve_scroll_in_band("test-bar", &runs, 2) {
            BandScrollOutcome::Hit(target) => {
                assert_eq!(target.plugin_id, "test-bar");
                assert_eq!(target.command, "scroll");
                assert_eq!(
                    target.args,
                    vec![("step".to_string(), ClickArg::Number(2.0))]
                );
            }
            outcome => panic!("expected scroll hit, got {outcome:?}"),
        }
        assert!(matches!(
            resolve_scroll_in_band("test-bar", &runs, 4),
            BandScrollOutcome::Denied
        ));
        assert!(matches!(
            resolve_scroll_in_band("test-bar", &runs, 99),
            BandScrollOutcome::NoClaim
        ));
    }

    #[test]
    fn scroll_args_table_merges_delta_last() {
        // CTX-1190 (#2008): dispatch carries declared args in order plus
        // the host delta; a declared delta is overwritten, never duplicated.
        let table = band_scroll_args_table(
            &[
                ("id".to_string(), ClickArg::String("vol".to_string())),
                ("delta".to_string(), ClickArg::Number(99.0)),
            ],
            -3,
            None,
        );
        let LuaValue::Table(pairs) = table else {
            panic!("scroll args must marshal as a table");
        };
        assert_eq!(pairs.len(), 2, "declared delta is replaced, not doubled");
        assert_eq!(
            pairs[0],
            (
                LuaValue::String("id".to_string()),
                LuaValue::String("vol".to_string())
            )
        );
        assert_eq!(
            pairs[1],
            (
                LuaValue::String(UI_SCROLL_DELTA_ARG.to_string()),
                LuaValue::Number(-3.0)
            )
        );
    }

    #[test]
    fn scroll_queue_coalesces_bursts_and_drops_overflow() {
        // CTX-1190 (#2008) burst acceptance at the queue level: consecutive
        // ticks for one target merge with saturating math; distinct targets
        // queue apart; overflow drops the newest and counts it.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let target = || BandScrollRequest {
            plugin_id: "test-bar".to_string(),
            command: "scroll".to_string(),
            args: vec![("step".to_string(), ClickArg::Number(2.0))],
            region: None,
            delta: 3,
            origin: None,
            admission_seq: 0,
        };
        for _ in 0..100 {
            rt.queue_band_scroll(target());
        }
        assert_eq!(
            rt.drain_band_scrolls().len(),
            1,
            "a 100-tick burst drains as one request"
        );
        let stats = rt.band_host_stats();
        assert_eq!(stats.scrolls_coalesced, 99);
        assert_eq!(stats.scrolls_routed, 1);
        assert_eq!(stats.scroll_queue_drops, 0);
        // Saturating math: no burst can overflow the step.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        rt.queue_band_scroll(BandScrollRequest {
            delta: i32::MAX,
            ..target()
        });
        rt.queue_band_scroll(target());
        let drained = rt.drain_band_scrolls();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].delta, i32::MAX, "delta saturates, never wraps");
        // Distinct targets queue apart; overflow drops the newest.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        for index in 0..BAND_SCROLL_QUEUE_MAX {
            rt.queue_band_scroll(BandScrollRequest {
                command: format!("scroll-{index}"),
                ..target()
            });
        }
        rt.queue_band_scroll(BandScrollRequest {
            command: "scroll-overflow".to_string(),
            ..target()
        });
        let drained = rt.drain_band_scrolls();
        assert_eq!(drained.len(), BAND_SCROLL_QUEUE_MAX);
        assert!(
            drained
                .iter()
                .all(|request| request.command != "scroll-overflow"),
            "overflow drops the newest request"
        );
        assert_eq!(rt.band_host_stats().scroll_queue_drops, 1);
        // A zero step never queues.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        rt.queue_band_scroll(BandScrollRequest {
            delta: 0,
            ..target()
        });
        assert!(rt.drain_band_scrolls().is_empty());
        assert_eq!(rt.band_host_stats().scrolls_routed, 0);
    }

    #[test]
    fn hidden_band_is_empty_text_only() {
        assert!(!band_is_visible(&text_node("")));
        assert!(!band_is_visible(&UiNode::row(vec![])));
        assert!(band_is_visible(&text_node(" ")));
        assert!(band_is_visible(&text_node("x")));
    }

    #[test]
    fn token_table_resolves_known_tokens() {
        let palette = ThemePalette::bitty_dark();
        assert_eq!(
            resolve_band_token("foreground", &palette),
            Some(palette.foreground)
        );
        assert_eq!(
            resolve_band_token("background", &palette),
            Some(palette.background)
        );
        assert_eq!(resolve_band_token("cursor", &palette), Some(palette.cursor));
        assert_eq!(
            resolve_band_token("selection", &palette),
            Some(palette.selection)
        );
        let accent = resolve_band_token("accent", &palette).expect("accent resolves");
        assert_eq!(accent[3], 0xFF, "accent is opaque");
        assert_ne!(accent, palette.foreground);
        assert_ne!(accent, palette.background);
        assert_eq!(
            resolve_band_token("normal", &palette),
            Some(palette.foreground)
        );
        assert_eq!(
            resolve_band_token("surface", &palette),
            Some(palette.selection)
        );
        for tok in [
            "muted",
            "warning",
            "critical",
            "ok",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "ansi.red",
            "ansi.cyan",
        ] {
            let resolved = resolve_band_token(tok, &palette).expect("semantic/ansi token resolves");
            assert_eq!(resolved[3], 0xFF);
        }
        assert_eq!(resolve_band_token("nope", &palette), None);
        assert_eq!(resolve_band_token("", &palette), None);
    }

    #[test]
    fn split_command_accepts_owner_qualified_and_short_forms() {
        assert_eq!(
            split_band_command("owner:focus", "owner").as_deref(),
            Some("focus")
        );
        assert_eq!(
            split_band_command("focus", "owner").as_deref(),
            Some("focus")
        );
        assert_eq!(split_band_command("other:focus", "owner"), None);
        assert_eq!(split_band_command("owner:", "owner"), None);
        assert_eq!(split_band_command("", "owner"), None);
        assert_eq!(split_band_command("a:b:c", "a"), None);
    }

    #[test]
    fn click_matrix_own_claim_outside_claim_overlapping_denied() {
        // Own claim routes to the owner's verb with declared args.
        let (_, runs) = flatten_band_runs(&UiNode::row(vec![
            text_node("1:ws1 "),
            click_node("2:ws2", "owner:focus"),
        ]));
        match resolve_click_in_band("owner", &runs, 7, BandButton::Left) {
            BandColumnOutcome::Hit(request) => {
                assert_eq!(request.plugin_id, "owner");
                assert_eq!(request.command, "focus");
                assert_eq!(
                    request.args,
                    vec![("id".to_string(), ClickArg::Number(3.0))]
                );
            }
            other => panic!("own claim must hit, got {other:?}"),
        }
        // Outside any claim (separator span, padding past the text).
        assert_eq!(
            resolve_click_in_band("owner", &runs, 2, BandButton::Left),
            BandColumnOutcome::NoClaim
        );
        assert_eq!(
            resolve_click_in_band("owner", &runs, 99, BandButton::Left),
            BandColumnOutcome::NoClaim
        );
        // Overlapping claim: a foreign qualifier on the span is denied,
        // never routed to either plugin.
        let (_, runs) = flatten_band_runs(&UiNode::row(vec![click_node("x", "other:focus")]));
        assert_eq!(
            resolve_click_in_band("owner", &runs, 0, BandButton::Left),
            BandColumnOutcome::Denied
        );
    }

    #[test]
    fn row_overlap_backstop_denies_duplicates() {
        assert!(!band_rows_overlap(&[0, 22]));
        assert!(!band_rows_overlap(&[]));
        assert!(band_rows_overlap(&[5, 5]));
        assert!(band_rows_overlap(&[0, 1, 1, 2]));
    }

    #[test]
    fn args_table_carries_named_values_in_order() {
        let args = vec![
            ("id".to_string(), ClickArg::Number(3.0)),
            ("name".to_string(), ClickArg::String("ws".to_string())),
            ("flag".to_string(), ClickArg::Bool(true)),
        ];
        let LuaValue::Table(pairs) = band_click_args_table(&args, None, BandButton::Left) else {
            panic!("args must encode as one table");
        };
        assert_eq!(
            pairs.len(),
            3,
            "a legacy region-less click keeps its pre-2248 payload"
        );
        assert_eq!(pairs[0].0, LuaValue::String("id".to_string()));
        assert_eq!(pairs[0].1, LuaValue::Number(3.0));
        assert_eq!(pairs[1].1, LuaValue::String("ws".to_string()));
        assert_eq!(pairs[2].1, LuaValue::Bool(true));
    }

    #[test]
    fn args_tables_merge_region_and_button_last() {
        // Issue #2248: region and button are host values, merged last and
        // overwriting declared entries of the same names (never doubled).
        let region = UiRegion {
            id: "clock".to_string(),
            kind: bitty_lua::ui::UiRegionKind::Module,
        };
        let declared = [
            ("region".to_string(), ClickArg::String("fake".to_string())),
            ("button".to_string(), ClickArg::String("fake".to_string())),
            ("id".to_string(), ClickArg::Number(3.0)),
        ];
        let LuaValue::Table(pairs) =
            band_click_args_table(&declared, Some(&region), BandButton::Middle)
        else {
            panic!("click args must encode as one table");
        };
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].0, LuaValue::String("id".to_string()));
        assert_eq!(
            pairs[1],
            (
                LuaValue::String(UI_REGION_ARG.to_string()),
                LuaValue::String("clock".to_string())
            )
        );
        assert_eq!(
            pairs[2],
            (
                LuaValue::String(UI_BUTTON_ARG.to_string()),
                LuaValue::String(UI_BUTTON_MIDDLE.to_string())
            )
        );

        let scroll_declared = [
            ("region".to_string(), ClickArg::String("fake".to_string())),
            ("id".to_string(), ClickArg::Number(3.0)),
        ];
        let LuaValue::Table(pairs) = band_scroll_args_table(&scroll_declared, -2, Some(&region))
        else {
            panic!("scroll args must encode as one table");
        };
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].0, LuaValue::String("id".to_string()));
        assert_eq!(
            pairs[1],
            (
                LuaValue::String(UI_REGION_ARG.to_string()),
                LuaValue::String("clock".to_string())
            )
        );
        assert_eq!(
            pairs[2],
            (
                LuaValue::String(UI_SCROLL_DELTA_ARG.to_string()),
                LuaValue::Number(-2.0)
            )
        );

        // A legacy scroll binding without a declared region keeps its own
        // `region` argument; the host only owns it for region-aware spans.
        let legacy = [
            ("region".to_string(), ClickArg::String("keep".to_string())),
            ("id".to_string(), ClickArg::Number(1.0)),
        ];
        let LuaValue::Table(pairs) = band_scroll_args_table(&legacy, -1, None) else {
            panic!("scroll args must encode as one table");
        };
        assert_eq!(pairs.len(), 3);
        assert_eq!(
            pairs[0],
            (
                LuaValue::String("region".to_string()),
                LuaValue::String("keep".to_string())
            )
        );
        assert_eq!(
            pairs[2],
            (
                LuaValue::String(UI_SCROLL_DELTA_ARG.to_string()),
                LuaValue::Number(-1.0)
            )
        );
    }

    #[test]
    fn cold_selection_bound_is_sixteen_and_leaves_the_tail_queued() {
        // CTX-1332 C-D9/C-D10: the bound is the literal 16 derived from the
        // existing click+scroll caps and is a selection bound, never a
        // mem::take drain. Queue one over the bound and prove the tail stays.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let origin = BandOrigin {
            plugin_id: "test-bar".to_string(),
            generation: 1,
            handle: 1,
            slot: UiSlot::Top,
            version: 1,
            edge: BandEdge::Top,
            visible_index: 0,
            row: 0,
            column: 0,
        };
        for seq in 0..COLD_ADMISSION_SELECTION_MAX {
            rt.band_click_queue.push(BandClickRequest {
                plugin_id: "test-bar".to_string(),
                command: format!("click-{seq}"),
                args: Vec::new(),
                region: None,
                button: BandButton::Left,
                origin: Some(origin.clone()),
                admission_seq: seq as u64,
            });
        }
        rt.band_click_queue.push(BandClickRequest {
            plugin_id: "test-bar".to_string(),
            command: "click-over".to_string(),
            args: Vec::new(),
            region: None,
            button: BandButton::Left,
            origin: Some(origin),
            admission_seq: COLD_ADMISSION_SELECTION_MAX as u64,
        });
        assert_eq!(rt.band_click_queue.len(), COLD_ADMISSION_SELECTION_MAX + 1);
        let selected = rt.drain_band_admissions(COLD_ADMISSION_SELECTION_MAX);
        assert_eq!(selected.len(), COLD_ADMISSION_SELECTION_MAX);
        assert_eq!(
            rt.band_click_queue.len(),
            1,
            "one-over work stays queued, never dropped"
        );
        assert_eq!(rt.band_click_queue[0].command, "click-over");
    }

    fn hover_node(text: &str, command: &str, region: &str) -> UiNode {
        UiNode::Text {
            anchor: None,
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
            on_hover: Some(ClickCommand {
                command: command.to_string(),
                args: Vec::new(),
                region: Some(UiRegion {
                    id: region.to_string(),
                    kind: bitty_lua::ui::UiRegionKind::Module,
                }),
            }),
        }
    }

    #[test]
    fn anchored_groups_center_and_flush_at_the_inset_edges() {
        // Issue #2411: true centering and a flush right edge, independent of
        // how wide the left group is (as long as the middle is free).
        let root = UiNode::row(vec![
            UiNode::row(vec![text_node("left")]),
            UiNode::Row {
                anchor: Some(UiAnchor::Center),
                align: None,
                children: vec![text_node("mid")],
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
                on_scroll: None,
                on_hover: None,
            },
            UiNode::Row {
                anchor: Some(UiAnchor::Right),
                align: None,
                children: vec![text_node("rt")],
                fg: None,
                bg: None,
                bold: None,
                on_click: None,
                on_scroll: None,
                on_hover: None,
            },
        ]);
        let (text, runs) = flatten_band_runs_in_width(&root, 40);
        assert_eq!(text, "leftmidrt");
        let columns: Vec<(usize, usize)> = runs
            .iter()
            .map(|run| (run.start_col, run.end_col))
            .collect();
        assert_eq!(columns, vec![(0, 4), (18, 21), (38, 40)]);
        assert_eq!((40 - 3) / 2, 18, "center is centered on the full width");

        // Narrower than the left group: the right group keeps the trailing
        // cell and the center is clipped away, never painted over the left.
        let (_, runs) = flatten_band_runs_in_width(&root, 3);
        for window in runs.windows(2) {
            assert!(window[0].end_col <= window[1].start_col);
        }
        assert!(runs.iter().all(|run| run.end_col <= 3));
    }

    #[test]
    fn hover_binding_inherits_to_descendant_runs() {
        let root = UiNode::Row {
            anchor: None,
            align: None,
            children: vec![text_node("clock")],
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
            on_hover: Some(ClickCommand {
                command: "test-bar:detail".to_string(),
                args: Vec::new(),
                region: Some(UiRegion {
                    id: "clock".to_string(),
                    kind: bitty_lua::ui::UiRegionKind::Module,
                }),
            }),
        };
        let (_, runs) = flatten_band_runs(&root);
        let binding = runs[0].on_hover.as_ref().expect("inherited hover");
        assert_eq!(binding.command, "test-bar:detail");
        assert_eq!(binding.region.as_ref().expect("region").id, "clock");
        match resolve_hover_in_band("test-bar", &runs, 0) {
            BandHoverOutcome::Target(target) => {
                assert_eq!(target.command, "detail");
                assert_eq!(target.region.id, "clock");
            }
            outcome => panic!("expected hover target, got {outcome:?}"),
        }
        assert!(matches!(
            resolve_hover_in_band("other-plugin", &runs, 0),
            BandHoverOutcome::Denied
        ));
    }

    #[test]
    fn hover_args_overwrite_declared_region_and_phase() {
        // The host keys win: a declared duplicate is replaced, never emitted
        // twice, and the arg budget the parser reserved still holds.
        let region = UiRegion {
            id: "clock".to_string(),
            kind: bitty_lua::ui::UiRegionKind::Module,
        };
        let args = vec![
            ("region".to_string(), ClickArg::String("spoof".to_string())),
            ("phase".to_string(), ClickArg::String("spoof".to_string())),
            ("label".to_string(), ClickArg::String("detail".to_string())),
        ];
        let table = band_hover_args_table(&args, &region, BandHoverPhase::Leave);
        let LuaValue::Table(pairs) = table else {
            panic!("hover args are a table");
        };
        assert_eq!(pairs.len(), 3, "no duplicate keys");
        let string_value = |name: &str| {
            pairs.iter().find_map(|(key, value)| match (key, value) {
                (LuaValue::String(key), LuaValue::String(value)) if key == name => {
                    Some(value.as_str())
                }
                _ => None,
            })
        };
        assert_eq!(string_value("label"), Some("detail"));
        assert_eq!(string_value(UI_REGION_ARG), Some("clock"));
        assert_eq!(string_value(UI_HOVER_PHASE_ARG), Some(UI_HOVER_PHASE_LEAVE));
    }

    #[test]
    fn pua_glyph_widths_are_one_cell_each() {
        // Icon maps are plugin data; Core only owes the existing text-cell
        // rule, so a Nerd Font code point keeps a deterministic one-cell
        // width and its module's hit region matches the paint.
        for glyph in ['\u{e800}', '\u{f017}', '\u{e0b0}', '\u{f000}'] {
            assert_eq!(
                usize::from(bitty_term_state::char_cell_width(glyph)),
                1,
                "PUA glyph U+{:04X} keeps one cell",
                glyph as u32
            );
        }
        let root = UiNode::row(vec![text_node("\u{e800}\u{f017} 80%")]);
        let (_, runs) = flatten_band_runs(&root);
        assert_eq!(runs[0].end_col, 6, "two icons plus a space and label");

        // A representative hoverable icon module resolves at the glyph cell.
        let icon = UiNode::row(vec![hover_node("\u{e800}", "test-bar:detail", "clock")]);
        let (_, runs) = flatten_band_runs(&icon);
        match resolve_hover_in_band("test-bar", &runs, 0) {
            BandHoverOutcome::Target(target) => assert_eq!(target.region.id, "clock"),
            outcome => panic!("expected icon hover target, got {outcome:?}"),
        }
    }

    #[test]
    fn hover_queue_fences_the_presented_origin() {
        // Review N3: hover uses the same presented-frame fence as clicks and
        // scrolls; a stale origin is refused instead of retargeted.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        rt.set_chrome_bands(ChromeBands {
            top: vec![super::BandContent {
                plugin_id: "test-bar".to_string(),
                slot: UiSlot::Top,
                root: UiNode::text("clock"),
                version: 1,
            }],
            ..Default::default()
        });
        let row = rt.visible_band_row(BandEdge::Top, 0).expect("band row");
        let origin = BandOrigin {
            plugin_id: "test-bar".to_string(),
            generation: 1,
            handle: 1,
            slot: UiSlot::Top,
            version: 1,
            edge: BandEdge::Top,
            visible_index: 0,
            row,
            column: 0,
        };
        let request = |origin: Option<BandOrigin>| BandHoverRequest {
            plugin_id: "test-bar".to_string(),
            command: "detail".to_string(),
            args: Vec::new(),
            region: UiRegion {
                id: "clock".to_string(),
                kind: bitty_lua::ui::UiRegionKind::Module,
            },
            phase: BandHoverPhase::Enter,
            origin,
        };

        // Not pending/published yet: stale, refused.
        rt.queue_band_hover(request(Some(origin.clone())));
        assert!(rt.band_hover_queue.is_empty());
        assert_eq!(rt.band_stats.hovers_denied, 1);

        // Live capture: accepted, and the app can dispatch it.
        rt.pending_band_origins = vec![origin.clone()];
        rt.published_band_origins = vec![origin.clone()];
        rt.queue_band_hover(request(Some(origin)));
        assert_eq!(rt.band_hover_queue.len(), 1);
        assert_eq!(rt.band_stats.hovers_routed, 1);
    }

    // -----------------------------------------------------------------------
    // Column bands (issue #3165)
    // -----------------------------------------------------------------------

    /// One column band whose root stacks `rows` top-down.
    fn column_block(plugin: &str, slot: UiSlot, rows: &[&str]) -> BandContent {
        BandContent {
            plugin_id: plugin.to_string(),
            slot,
            root: UiNode::column(rows.iter().map(|row| UiNode::text(*row)).collect()),
            version: 1,
        }
    }

    #[test]
    fn column_band_width_measures_the_widest_root_row() {
        // The scene decides the band thickness: the widest direct child, not
        // the sum (a sum would make every vertical band the whole window).
        let band = column_block("test-bar", UiSlot::Left, &["ab", "abcdef", "abc"]);
        assert_eq!(column_band_width(&band.root), 6);
        // A nested `Row` child is measured as one row, joined horizontally.
        let nested = UiNode::column(vec![UiNode::row(vec![
            UiNode::text("ab"),
            UiNode::text("cde"),
        ])]);
        assert_eq!(column_band_width(&nested), 5);
        // The axis-neutral single-line root.
        assert_eq!(column_band_width(&UiNode::text("abcd")), 4);
        // A hidden band measures zero; it never reserves columns.
        assert_eq!(column_band_width(&UiNode::column(Vec::new())), 0);
        // Wide cells are two cells wide, so the band never clips them.
        assert_eq!(column_band_width(&UiNode::text("日本")), 4);
    }

    #[test]
    fn column_band_rows_stack_one_row_per_child_in_order() {
        let root = UiNode::column(vec![
            UiNode::text("cpu"),
            UiNode::text("mem"),
            UiNode::text("net"),
        ]);
        let rows = column_band_rows(&root, 8);
        let text: Vec<&str> = rows.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(
            text,
            ["cpu", "mem", "net"],
            "declaration order is paint order"
        );
        // The row budget bounds the work: rows past the band clip away
        // without flattening the rest of the scene.
        let bounded = column_band_rows(&root, 2);
        assert_eq!(bounded.len(), 2);
        assert_eq!(column_band_rows(&root, 0).len(), 0);
        // A `List` root stacks identically.
        let list = UiNode::list(vec![UiNode::text("a"), UiNode::text("b")]);
        assert_eq!(column_band_rows(&list, 4).len(), 2);
        // The axis-neutral `Text` root is one row.
        let single = column_band_rows(&UiNode::text("only"), 4);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].0, "only");
    }

    #[test]
    fn column_bands_reserve_columns_from_the_window_edge() {
        // Acceptance 1: a mounted `left` scene owns a bounded band and its
        // columns leave the layout container, exactly as a row band takes
        // rows.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let window = rt.window_cells();
        let band = column_block("test-bar", UiSlot::Left, &["cpu 42%", "mem 30%"]);
        let width = column_band_width(&band.root);
        assert_eq!(width, 7);
        rt.set_chrome_bands(ChromeBands {
            left: vec![band],
            ..Default::default()
        });

        assert_eq!(rt.plugin_band_column_budget(), (width, 0));
        assert_eq!(rt.visible_band_column_count(BandEdge::Left), 1);
        assert_eq!(rt.visible_band_column(BandEdge::Left, 0), Some(0));
        assert_eq!(rt.visible_band_column(BandEdge::Left, 1), None);
        // No row geometry appears for a column band.
        assert_eq!(rt.visible_band_count(BandEdge::Left), 0);
        assert_eq!(rt.visible_band_row(BandEdge::Left, 0), None);

        let container = rt.band_exclusive_container();
        assert_eq!(
            container.width,
            window.width - width,
            "content keeps the rest"
        );
        assert_eq!(container.height, window.height, "columns cost no rows");
        assert_eq!(container.x, window.x + width);
        // The row carve is untouched: a column band spans its rows.
        assert_eq!(rt.band_row_container().width, window.width);
    }

    #[test]
    fn right_column_bands_stack_inward_from_the_right_window_edge() {
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let window = rt.window_cells();
        // Plugin id order decides stacking, not mount order: `aaa` (2 wide)
        // sorts first and therefore sits outermost, at the window edge.
        let (bands, unplaced) = ChromeBands::from_mounts([
            (
                "bbb".to_string(),
                UiSlot::Right,
                UiNode::column(vec![UiNode::text("abcd")]),
                1,
            ),
            (
                "aaa".to_string(),
                UiSlot::Right,
                UiNode::column(vec![UiNode::text("xy")]),
                1,
            ),
        ]);
        assert_eq!(unplaced, 0);
        rt.set_chrome_bands(bands);
        assert_eq!(rt.plugin_band_column_budget(), (0, 6));
        // Index 0 is the outermost band: `aaa` occupies the last two columns,
        // `bbb` stacks inward of it.
        assert_eq!(
            rt.visible_band_column(BandEdge::Right, 0),
            Some(window.width - 2)
        );
        assert_eq!(
            rt.visible_band_column(BandEdge::Right, 1),
            Some(window.width - 6)
        );
        assert_eq!(
            rt.band_exclusive_container().width,
            window.width - 6,
            "both bands reserve columns"
        );
    }

    #[test]
    fn column_band_wider_than_its_share_is_clipped_to_the_content_half() {
        // Issue #3171 decision: a scene wider than its share is *clipped to
        // the share*, not degraded to an invisible band. #3165 returned
        // `(0, 0)` here, so a plugin held a band that reserved, painted, and
        // routed nothing; a 79-column line in an 80-column window also left a
        // one-column terminal. The reservation is now bounded instead, so the
        // mount is served at the bound and the content keeps half the window.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let window = rt.window_cells();
        let too_wide = "x".repeat(usize::from(window.width).saturating_sub(1));
        rt.set_chrome_bands(ChromeBands {
            left: vec![column_block("test-bar", UiSlot::Left, &[&too_wide])],
            ..Default::default()
        });
        let share = window.width / BAND_COLUMN_CONTENT_SHARE_DIVISOR;
        assert_eq!(
            rt.plugin_band_column_budget(),
            (share, 0),
            "a band wider than its share is granted the share, not zero columns"
        );
        assert_eq!(rt.visible_band_column_count(BandEdge::Left), 1);
        assert_eq!(rt.visible_band_column(BandEdge::Left, 0), Some(0));
        let container = rt.band_exclusive_container();
        assert_eq!(
            container.width,
            window.width - share,
            "the content keeps at least half the window"
        );
        assert!(
            container.width >= crate::runtime::chrome_band::MIN_CONTENT_COLS,
            "the content floor still holds"
        );
        // The band still paints and still routes: it is bounded, not invisible.
        assert!(rt.band_item_regions(BandEdge::Left, 0).is_ok());
    }

    #[test]
    fn column_band_degrades_only_when_the_window_has_no_spare_column() {
        // The one remaining `(0, 0)` is a window with no column to share at
        // all, which is the same degradation a row band takes in a window too
        // short for it — not the "admitted then invisible" state #3165 left.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let window = rt.window_cells();
        rt.handle_resize(bitty_platform::PhysicalSize::new(
            rt.live_cell_size().0,
            rt.live_cell_size()
                .1
                .saturating_mul(u32::from(window.height)),
        ))
        .expect("resize to one column");
        assert_eq!(rt.window_cells().width, 1, "one-column window");
        rt.set_chrome_bands(ChromeBands {
            left: vec![column_block("test-bar", UiSlot::Left, &["x"])],
            ..Default::default()
        });
        assert_eq!(rt.plugin_band_column_budget(), (0, 0));
        assert_eq!(rt.visible_band_column_count(BandEdge::Left), 0);
        assert_eq!(
            rt.band_exclusive_container(),
            rt.window_cells(),
            "no reservation"
        );
    }

    #[test]
    fn row_band_geometry_is_unchanged_beside_a_column_band() {
        // Acceptance 3: `top`/`bottom` keep their rows, budget, and height
        // carve while a vertical band reserves columns.
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let window = rt.window_cells();
        let row_band = BandContent {
            plugin_id: "test-bar".to_string(),
            slot: UiSlot::Top,
            root: UiNode::row(vec![UiNode::text("clock")]),
            version: 1,
        };
        rt.set_chrome_bands(ChromeBands {
            top: vec![row_band],
            ..Default::default()
        });
        let rows_only = rt.plugin_band_budget();
        let row_only = rt.visible_band_row(BandEdge::Top, 0);
        let container_rows_only = rt.band_exclusive_container();
        assert_eq!(rows_only, (1, 0));
        assert_eq!(row_only, Some(window.y));

        rt.set_chrome_bands(ChromeBands {
            top: vec![BandContent {
                plugin_id: "test-bar".to_string(),
                slot: UiSlot::Top,
                root: UiNode::row(vec![UiNode::text("clock")]),
                version: 1,
            }],
            left: vec![column_block("side-bar", UiSlot::Left, &["cpu"])],
            ..Default::default()
        });
        assert_eq!(rt.plugin_band_budget(), rows_only, "row budget unchanged");
        assert_eq!(
            rt.visible_band_row(BandEdge::Top, 0),
            row_only,
            "top band keeps its row"
        );
        let container = rt.band_exclusive_container();
        assert_eq!(container.height, container_rows_only.height);
        assert_eq!(container.y, container_rows_only.y);
        assert_eq!(
            container.width,
            container_rows_only.width - 3,
            "only the column band's width leaves the container"
        );
    }

    #[test]
    fn multi_band_panel_sheds_duplicate_rows_before_evicting_single_band_bar() {
        let mut rt = Runtime::with_defaults().expect("runtime must build");
        let floor = rt.chrome_min_rows();
        let spare: u16 = 5;
        let window_rows = floor.saturating_add(spare);
        rt.set_window_cells(bitty_ui::Rect::new(0, 0, 80, window_rows));

        let mut mounts = Vec::new();
        for row in 0..8 {
            mounts.push((
                "bitty-featured.devtools".to_string(),
                UiSlot::Bottom,
                UiNode::row(vec![UiNode::text(format!("devtools-row-{row}"))]),
                1,
            ));
        }
        mounts.push((
            "bitty-terminal.bar".to_string(),
            UiSlot::Bottom,
            UiNode::row(vec![UiNode::text("bar-clock")]),
            1,
        ));
        let (bands, unplaced) = ChromeBands::from_mounts(mounts);
        assert_eq!(unplaced, 0);
        rt.set_chrome_bands(bands);
        let _ = rt.tick();

        assert_eq!(
            rt.plugin_band_budget(),
            (0, spare),
            "short window retains `spare` rows instead of dropping all horizontal bands"
        );
        let kept = rt.visible_edge_bands(BandEdge::Bottom);
        assert_eq!(kept.len(), usize::from(spare));
        assert!(
            kept.iter()
                .any(|band| band.plugin_id == "bitty-terminal.bar"),
            "single-band bar must survive duplicate-row shedding from multi-band panel: {kept:?}"
        );
        assert!(
            rt.band_row_container().height >= floor,
            "content row floor stays intact"
        );
        assert!(!rt.band_geometry_violated());
    }
}
