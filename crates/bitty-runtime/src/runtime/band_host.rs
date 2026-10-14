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
//! Ownership direction follows the accepted W-74 disposition (Core owns
//! mechanism — geometry, routing, confinement; plugins own presentation)
//! and the W-82 spelling discipline: no new capability, no new slot, no new
//! error code. The only new host surface is the drain queue
//! ([`Runtime::drain_band_clicks`]) plus headless statistics, consistent
//! with the overlay/capture conventions (CTX-0941: Core holds the switch,
//! the application moves the bytes).

use super::band_slots::BandEdge;
use super::{BandContent, Runtime};
use bitty_lua::host::LuaValue;
use bitty_lua::ui::{ClickArg, ClickCommand, UI_SCROLL_DELTA_ARG, UiNode, UiSlot};
use bitty_platform::CursorPosition;
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
/// [`Runtime::queue_band_scroll`]), so the queue holds one frame of scrolls
/// the application has not drained yet. Overflow drops the newest request
/// fail-closed and counts [`BandHostStats::scroll_queue_drops`].
pub const BAND_SCROLL_QUEUE_MAX: usize = 8;

/// ANSI index resolving the `accent` band token (CTX-0946 C2).
///
/// The terminal's conventional accent: bright blue, OSC-4-aware through
/// [`ThemePalette::active_index`] so a user remap moves the accent with the
/// rest of the palette. The candidate theme-token contract will own the
/// full vocabulary; until it is accepted this table is the whole host
/// surface.
pub const BAND_ACCENT_ANSI_INDEX: u8 = 12;

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
}

/// Flattens a band tree into its full text plus styled runs (CTX-0946).
///
/// `Row` children join horizontally; `Column`/`List` children join the same
/// way (vertical stacking is deferred, matching the paint path). The text is
/// the concatenation of every `Text` leaf in walk order; each leaf yields
/// exactly one run, even when empty. Total and headless.
#[must_use]
pub fn flatten_band_runs(root: &UiNode) -> (String, Vec<BandRun>) {
    let mut text = String::new();
    let mut runs = Vec::new();
    flatten_into(root, &mut text, &mut runs, None, None, None, None, false);
    (text, runs)
}

#[allow(clippy::too_many_arguments)]
fn flatten_into(
    node: &UiNode,
    text: &mut String,
    runs: &mut Vec<BandRun>,
    fg: Option<String>,
    bg: Option<String>,
    on_click: Option<ClickCommand>,
    on_scroll: Option<ClickCommand>,
    bold: bool,
) {
    match node {
        UiNode::Text {
            text: leaf,
            fg: leaf_fg,
            bg: leaf_bg,
            bold: leaf_bold,
            on_click: leaf_click,
            on_scroll: leaf_scroll,
        } => {
            let start = display_cells(text);
            let start_char = text.chars().count();
            text.push_str(leaf);
            runs.push(BandRun {
                start_col: start,
                end_col: display_cells(text),
                start_char,
                end_char: text.chars().count(),
                fg: leaf_fg.clone().or(fg),
                bg: leaf_bg.clone().or(bg),
                bold: leaf_bold.unwrap_or(bold),
                on_click: leaf_click.clone().or(on_click),
                on_scroll: leaf_scroll.clone().or(on_scroll),
            });
        }
        UiNode::Row {
            children,
            fg: row_fg,
            bg: row_bg,
            bold: row_bold,
            on_click: row_click,
            on_scroll: row_scroll,
        }
        | UiNode::Column {
            children,
            fg: row_fg,
            bg: row_bg,
            bold: row_bold,
            on_click: row_click,
            on_scroll: row_scroll,
        }
        | UiNode::List {
            children,
            fg: row_fg,
            bg: row_bg,
            bold: row_bold,
            on_click: row_click,
            on_scroll: row_scroll,
        } => {
            let fg = row_fg.clone().or(fg);
            let bg = row_bg.clone().or(bg);
            let bold = row_bold.unwrap_or(bold);
            let on_click = row_click.clone().or(on_click);
            let on_scroll = row_scroll.clone().or(on_scroll);
            for child in children {
                flatten_into(
                    child,
                    text,
                    runs,
                    fg.clone(),
                    bg.clone(),
                    on_click.clone(),
                    on_scroll.clone(),
                    bold,
                );
            }
        }
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

/// Minimal host theme-token table for band paint (CTX-0946 C2).
///
/// Accepted tokens today: `foreground`, `background`, `cursor`,
/// `selection`, and `accent` (bright-blue ANSI, OSC-4-aware). Returns `None`
/// for anything else; callers substitute the span default and count
/// [`BandHostStats::unknown_tokens`]. The table is deliberately closed: the
/// candidate theme-token contract owns any wider vocabulary.
#[must_use]
pub fn resolve_band_token(token: &str, palette: &ThemePalette) -> Option<Rgba8> {
    match token {
        "foreground" => Some(palette.foreground),
        "background" => Some(palette.background),
        "cursor" => Some(palette.cursor),
        "selection" => Some(palette.selection),
        "accent" => {
            let rgb = palette.active_index(BAND_ACCENT_ANSI_INDEX);
            Some([rgb[0], rgb[1], rgb[2], 0xFF])
        }
        _ => None,
    }
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
/// table (`run({id = 3})`), preserving declaration order.
#[must_use]
pub fn band_click_args_table(args: &[(String, ClickArg)]) -> LuaValue {
    LuaValue::Table(
        args.iter()
            .map(|(key, value)| {
                let value = match value {
                    ClickArg::String(text) => LuaValue::String(text.clone()),
                    ClickArg::Number(number) => LuaValue::Number(*number),
                    ClickArg::Bool(flag) => LuaValue::Bool(*flag),
                };
                (LuaValue::String(key.clone()), value)
            })
            .collect(),
    )
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
/// Pure over one band: finds the run owning `char_idx` (display-cell
/// column) and splits its command against `owner`. Pure for headless tests;
/// geometry (which band owns the row) lives on [`Runtime::plugin_band_hit`].
#[must_use]
pub fn resolve_click_in_band(owner: &str, runs: &[BandRun], char_idx: usize) -> BandColumnOutcome {
    let Some(run) = find_run_index(runs, char_idx).and_then(|index| runs.get(index)) else {
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
    })
}

/// Index of the run owning a display-cell column, if any (CTX-0946 C1).
///
/// Pure run identity behind [`resolve_click_in_band`]: press and release
/// compare run indices (plus band and resolved request) so a press-drag
/// onto another span can never fire its command.
#[must_use]
pub fn find_run_index(runs: &[BandRun], char_idx: usize) -> Option<usize> {
    runs.iter()
        .position(|run| char_idx >= run.start_col && char_idx < run.end_col)
}

/// One Core-routed band scroll: owner, verb, declared args, and the
/// host-computed line step (CTX-1190, issue #2008).
///
/// Like [`BandClickRequest`] the plugin never sees pointer coordinates —
/// only this declared triple plus `delta`, dispatched by the application
/// through `PluginRuntime::dispatch_command`, whose registration and
/// capability gates fail closed as usual. `delta` is the signed wheel step
/// (direction x lines, positive for wheel-up); the dispatch table carries it
/// under [`UI_SCROLL_DELTA_ARG`](bitty_lua::ui::UI_SCROLL_DELTA_ARG),
/// overwriting a declared entry of the same name.
#[derive(Debug, Clone, PartialEq)]
pub struct BandScrollRequest {
    /// Owning plugin id (the band that was hit).
    pub plugin_id: String,
    /// Command verb, unqualified (`scroll`, never `other-plugin:verb`).
    pub command: String,
    /// Declared `on_scroll.args` in declaration order.
    pub args: Vec<(String, ClickArg)>,
    /// Host-computed signed line step for this request.
    pub delta: i32,
}

/// Converts declared scroll args plus the host-computed delta to the single
/// Lua table arg (CTX-1190, issue #2008).
///
/// Mirrors [`band_click_args_table`]: `dispatch_command` takes positional
/// args; a scroll carries one named table (`run({id = 3, delta = 1})`),
/// preserving declaration order with `delta` merged last. A declared
/// `delta` entry is overwritten by the host value, never duplicated.
#[must_use]
pub fn band_scroll_args_table(args: &[(String, ClickArg)], delta: i32) -> LuaValue {
    let mut table: Vec<(LuaValue, LuaValue)> = args
        .iter()
        .filter(|(key, _)| key != UI_SCROLL_DELTA_ARG)
        .map(|(key, value)| {
            let value = match value {
                ClickArg::String(text) => LuaValue::String(text.clone()),
                ClickArg::Number(number) => LuaValue::Number(*number),
                ClickArg::Bool(flag) => LuaValue::Bool(*flag),
            };
            (LuaValue::String(key.clone()), value)
        })
        .collect();
    table.push((
        LuaValue::String(UI_SCROLL_DELTA_ARG.to_string()),
        LuaValue::Number(f64::from(delta)),
    ));
    LuaValue::Table(table)
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
}

/// Resolves a flattened-text column into its scroll outcome (CTX-1190).
///
/// Pure over one band: finds the run owning `char_idx` (display-cell
/// column) and splits its `on_scroll` command against `owner`, mirroring
/// [`resolve_click_in_band`]. Geometry (which band owns the row) lives on
/// [`Runtime::plugin_band_hit`]; the wheel step joins in
/// [`Runtime::band_scroll`].
#[must_use]
pub fn resolve_scroll_in_band(owner: &str, runs: &[BandRun], char_idx: usize) -> BandScrollOutcome {
    let Some(run) = find_run_index(runs, char_idx).and_then(|index| runs.get(index)) else {
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
    })
}

/// Press-side band target stored by [`Runtime::band_press`] (CTX-0946 C1).
///
/// Standard button semantics: a release routes only onto the same target
/// the press armed (band, run, and resolved request all equal). Anything
/// else — a drag onto another span or band, or content that changed
/// mid-gesture — counts unclaimed (or denied, when the release itself is
/// denied on the same target the press armed).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PressedBandTarget {
    /// The press armed a claimed span: band, run index, resolved request.
    Hit {
        edge: BandEdge,
        visible_index: usize,
        run: usize,
        request: BandClickRequest,
    },
    /// The press landed on an unclaimed span or past the text.
    NoClaim,
    /// The press landed on an unroutable claim (foreign qualifier, ...).
    Denied,
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
    /// Display-cell column into the flattened band text.
    pub char_idx: usize,
}

/// A resolved band row: which visible band row owns the pointer.
///
/// Row-level only — no text column. Press consumption uses this so the
/// whole painted row (including the unclaimed trailing region past the
/// text) is Core chrome; release resolution narrows through
/// [`Runtime::plugin_band_hit`], whose text-width guard keeps a
/// past-text release at `NoClaim` instead of saturating into the final
/// span's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandRowHit {
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible index on the edge (hidden bands take no row).
    pub visible_index: usize,
    /// Window row the band paints (window cells).
    pub row: u16,
}

impl Runtime {
    /// Visible (non-empty-text) bands on `edge`, in stacking order.
    ///
    /// Hidden bands take no stacking row (CTX-0925 item 3): every consumer
    /// — row assignment, paint, hit-test, and the exclusive-zone budget —
    /// reads this list, so a hidden band can never own geometry.
    #[must_use]
    pub fn visible_edge_bands(&self, edge: BandEdge) -> Vec<&BandContent> {
        self.chrome_bands
            .edge(edge)
            .iter()
            .filter(|band| band_is_visible(&band.root))
            .collect()
    }

    /// Visible band rows reserved on horizontal `edge` (CTX-0946 C3).
    ///
    /// Counts visible bands only, clamped by the degradation budget
    /// ([`Self::plugin_band_budget`]): `0` when the window cannot hold the
    /// bands above the content floor, or for a vertical edge (vertical
    /// bands are stored, never painted).
    #[must_use]
    pub fn visible_band_count(&self, edge: BandEdge) -> u16 {
        let (top, bottom) = self.plugin_band_budget();
        match edge {
            BandEdge::Top => top,
            BandEdge::Bottom => bottom,
            BandEdge::Left | BandEdge::Right => 0,
        }
    }

    /// Degraded-visible band budget per horizontal edge (CTX-0946 C3).
    ///
    /// Visible band counts, or `(0, 0)` when the window is too small for
    /// every visible band plus the content floor. Pure and total; every
    /// geometry consumer reads this, so paint, hit-test, and the container
    /// can never disagree.
    ///
    /// CTX-0979: Core draws no workspace display, so there is no Core bar
    /// reservation; the budget degrades against the window rows directly.
    #[must_use]
    pub fn plugin_band_budget(&self) -> (u16, u16) {
        let count = |edge| {
            u16::try_from(
                self.chrome_bands
                    .edge(edge)
                    .iter()
                    .filter(|band| band_is_visible(&band.root))
                    .count(),
            )
            .unwrap_or(u16::MAX)
        };
        let (top, bottom) = (count(BandEdge::Top), count(BandEdge::Bottom));
        if top == 0 && bottom == 0 {
            return (0, 0);
        }
        let window = self.window_cells();
        let window_rows = u32::from(window.height);
        let want = u32::from(top).saturating_add(u32::from(bottom));
        let floor = u32::from(self.chrome_min_rows());
        if want.saturating_add(floor) > window_rows {
            return (0, 0);
        }
        (top, bottom)
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

    /// Layout container minus the budgeted plugin bands (CTX-0946 C3).
    ///
    /// CTX-0979: Core draws no workspace display, so the container starts
    /// from the full window grid; this carves the plugin exclusive zone out
    /// of it. Falls back to the full window when the budget hides every
    /// band, so the no-plugin frame is byte-identical to before. Saturating
    /// and total.
    #[must_use]
    pub fn band_exclusive_container(&self) -> bitty_ui::Rect {
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
    /// container. Paint and hit-test fail closed on `true` with a
    /// diagnostic; the budget math keeps this unreachable in practice.
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
        false
    }

    /// Maps physical `pos` onto a painted band row, if any (CTX-0946 C1).
    ///
    /// Row-wide: any column inside the band's window width hits, including
    /// the unclaimed trailing region past the text. Press consumption uses
    /// this so a band press can never fall through to selection or capture.
    /// Uses the same window-padding + live-cell translation the band paint
    /// path uses, so hit-test and paint share one geometry (no second
    /// source). Returns `None` outside every painted band row, past the
    /// window width, or under any geometry violation (fail-closed).
    #[must_use]
    pub fn plugin_band_row_hit(&self, pos: CursorPosition) -> Option<BandRowHit> {
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
        let band_w = f64::from(window.width) * cell_w;
        if pos.x < origin_x || pos.x >= origin_x + band_w {
            return None;
        }
        for edge in [BandEdge::Top, BandEdge::Bottom] {
            for index in 0..self.visible_band_count(edge) {
                let index = usize::from(index);
                let Some(row) = self.visible_band_row(edge, index) else {
                    continue;
                };
                let origin_y = pad + f64::from(row) * cell_h;
                if pos.y < origin_y || pos.y >= origin_y + cell_h {
                    continue;
                }
                return Some(BandRowHit {
                    edge,
                    visible_index: index,
                    row,
                });
            }
        }
        None
    }

    /// Maps physical `pos` onto a claimed band text column, if any
    /// (CTX-0946 C1).
    ///
    /// Release resolution over [`Self::plugin_band_row_hit`]: the row must
    /// hit, and the column must land inside the painted text width (the
    /// paint path clips text to the window width, so the guard mirrors it).
    /// Returns `None` past the text width — the caller counts that release
    /// as unclaimed, never as the final span's owner.
    #[must_use]
    pub fn plugin_band_hit(&self, pos: CursorPosition) -> Option<BandHit> {
        let row_hit = self.plugin_band_row_hit(pos)?;
        let live = self.live_cell_metrics();
        let window = self.window_cells();
        let pad = f64::from(self.window_padding_physical());
        let cell_w = f64::from(live.width);
        let origin_x = pad + f64::from(window.x) * cell_w;
        let cell_col = ((pos.x - origin_x) / cell_w).floor() as u64;
        let bands = self.visible_edge_bands(row_hit.edge);
        let band = bands.get(row_hit.visible_index)?;
        let (text, _) = flatten_band_runs(&band.root);
        let width_cells = u64::try_from(display_cells(&text)).unwrap_or(u64::MAX);
        let window_cells = u64::from(window.width);
        if cell_col >= width_cells.min(window_cells) {
            return None;
        }
        let char_idx = cell_col_to_char_idx(&text, usize::try_from(cell_col).unwrap_or(usize::MAX));
        Some(BandHit {
            edge: row_hit.edge,
            visible_index: row_hit.visible_index,
            row: row_hit.row,
            char_idx,
        })
    }

    /// Routes a left press on a plugin band row to chrome (CTX-0946 C1).
    ///
    /// Returns `true` (consume the press: no focus move, no selection, no
    /// capture report) when the last-known cursor sits anywhere on a painted
    /// band row — including the unclaimed trailing region past the text —
    /// arming the one-shot [`Self::band_release_swallow`] plus the stored
    /// press target so the paired release routes only onto the same band
    /// run (standard button semantics). Shift still forces the selection
    /// path (the CTX-0181 accessibility escape).
    pub(super) fn band_press(&mut self) -> bool {
        if self.shift_pressed {
            return false;
        }
        let Some(pos) = self.last_cursor else {
            return false;
        };
        let Some(row_hit) = self.plugin_band_row_hit(pos) else {
            return false;
        };
        self.band_press_target = Some(self.band_target_at(pos, &row_hit));
        self.band_release_swallow = true;
        true
    }

    /// Resolves a press or release position into its stored-target form.
    ///
    /// Shared by [`Self::band_press`] and [`Self::band_release`] so both
    /// sides classify identically: past-text is `NoClaim` (the width guard
    /// in [`Self::plugin_band_hit`], never the saturating tail of
    /// [`cell_col_to_char_idx`]).
    fn band_target_at(&self, pos: CursorPosition, row_hit: &BandRowHit) -> PressedBandTarget {
        let bands = self.visible_edge_bands(row_hit.edge);
        let Some(band) = bands.get(row_hit.visible_index) else {
            return PressedBandTarget::NoClaim;
        };
        let owner = band.plugin_id.clone();
        let (_, runs) = flatten_band_runs(&band.root);
        let Some(hit) = self.plugin_band_hit(pos) else {
            return PressedBandTarget::NoClaim;
        };
        if (hit.edge, hit.visible_index) != (row_hit.edge, row_hit.visible_index) {
            // Unreachable (same geometry walk twice); fail closed.
            return PressedBandTarget::NoClaim;
        }
        match resolve_click_in_band(&owner, &runs, hit.char_idx) {
            BandColumnOutcome::Hit(request) => {
                let Some(run) = find_run_index(&runs, hit.char_idx) else {
                    // Unreachable (a Hit always owns a run); fail closed.
                    return PressedBandTarget::NoClaim;
                };
                PressedBandTarget::Hit {
                    edge: row_hit.edge,
                    visible_index: row_hit.visible_index,
                    run,
                    request,
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
    pub(super) fn band_release(&mut self) -> bool {
        if !self.band_release_swallow {
            return false;
        }
        self.band_release_swallow = false;
        let pressed = self.band_press_target.take();
        let Some(pos) = self.last_cursor else {
            self.band_stats.clicks_unclaimed = self.band_stats.clicks_unclaimed.saturating_add(1);
            return true;
        };
        let Some(row_hit) = self.plugin_band_row_hit(pos) else {
            self.band_stats.clicks_unclaimed = self.band_stats.clicks_unclaimed.saturating_add(1);
            return true;
        };
        let released = self.band_target_at(pos, &row_hit);
        match (pressed, released) {
            (
                Some(PressedBandTarget::Hit {
                    edge,
                    visible_index,
                    run,
                    request,
                }),
                PressedBandTarget::Hit {
                    edge: release_edge,
                    visible_index: release_visible,
                    run: release_run,
                    request: release_request,
                },
            ) if edge == release_edge
                && visible_index == release_visible
                && run == release_run
                && request == release_request =>
            {
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
                self.queue_band_scroll(BandScrollRequest {
                    plugin_id: target.plugin_id,
                    command: target.command,
                    args: target.args,
                    delta,
                });
                true
            }
            ScrollTargetOutcome::Denied => {
                self.band_stats.scrolls_denied = self.band_stats.scrolls_denied.saturating_add(1);
                false
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
        let Some(row_hit) = self.plugin_band_row_hit(pos) else {
            return ScrollTargetOutcome::OffBand;
        };
        let bands = self.visible_edge_bands(row_hit.edge);
        let Some(band) = bands.get(row_hit.visible_index) else {
            return ScrollTargetOutcome::Unclaimed;
        };
        let owner = band.plugin_id.clone();
        let (_, runs) = flatten_band_runs(&band.root);
        let Some(hit) = self.plugin_band_hit(pos) else {
            return ScrollTargetOutcome::Unclaimed;
        };
        match resolve_scroll_in_band(&owner, &runs, hit.char_idx) {
            BandScrollOutcome::Hit(target) => ScrollTargetOutcome::Hit(target),
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

    /// Whether a band-consumed press still owns its release.
    #[must_use]
    pub fn band_release_armed(&self) -> bool {
        self.band_release_swallow
    }

    /// Version snapshot of every mounted band, in stacking order (CTX-0946 C2).
    ///
    /// The present path compares this against the last presented frame: a
    /// mount, update, or unmount forces a frame on a quiet grid (band damage
    /// only — geometry reflows separately through the exclusive-zone
    /// budget). Versions come from the mount registry (1 at mount,
    /// incremented by update), so content-equal rewrites still present once
    /// rather than risking a stale band.
    #[must_use]
    pub fn chrome_band_versions(&self) -> Vec<(String, UiSlot, u32)> {
        let mut versions = Vec::new();
        for edge in [
            BandEdge::Top,
            BandEdge::Bottom,
            BandEdge::Left,
            BandEdge::Right,
        ] {
            for band in self.chrome_bands.edge(edge) {
                versions.push((band.plugin_id.clone(), band.slot, band.version));
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

/// Maps a display-cell column to a char index in `text` (CTX-0946 C1).
///
/// Walks with [`char_cell_width`](bitty_term_state::char_cell_width) — the
/// same widths the paint path lays out — so a pointer column and its glyph
/// agree. Saturates at the last char for out-of-range columns.
fn cell_col_to_char_idx(text: &str, cell_col: usize) -> usize {
    let mut cells = 0usize;
    for (idx, ch) in text.chars().enumerate() {
        let width = usize::from(bitty_term_state::char_cell_width(ch));
        if cell_col < cells.saturating_add(width) {
            return idx;
        }
        cells = cells.saturating_add(width);
    }
    text.chars().count().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_node(text: &str) -> UiNode {
        UiNode::Text {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
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
            }),
            on_scroll: None,
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
            }),
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
        };
        let root = UiNode::Row {
            children: vec![text_node("x")],
            fg: Some("accent".to_string()),
            bg: None,
            bold: Some(true),
            on_click: Some(click.clone()),
            on_scroll: None,
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
            children: vec![UiNode::Text {
                text: "x".to_string(),
                fg: Some("foreground".to_string()),
                bg: None,
                bold: Some(false),
                on_click: None,
                on_scroll: None,
            }],
            fg: Some("accent".to_string()),
            bg: None,
            bold: Some(true),
            on_click: None,
            on_scroll: None,
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
        };
        let root = UiNode::Row {
            children: vec![text_node("x"), scroll_node("y", "owner:leaf")],
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: Some(scroll.clone()),
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
                }),
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
            delta: 3,
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
        match resolve_click_in_band("owner", &runs, 7) {
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
            resolve_click_in_band("owner", &runs, 2),
            BandColumnOutcome::NoClaim
        );
        assert_eq!(
            resolve_click_in_band("owner", &runs, 99),
            BandColumnOutcome::NoClaim
        );
        // Overlapping claim: a foreign qualifier on the span is denied,
        // never routed to either plugin.
        let (_, runs) = flatten_band_runs(&UiNode::row(vec![click_node("x", "other:focus")]));
        assert_eq!(
            resolve_click_in_band("owner", &runs, 0),
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
        let LuaValue::Table(pairs) = band_click_args_table(&args) else {
            panic!("args must encode as one table");
        };
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].0, LuaValue::String("id".to_string()));
        assert_eq!(pairs[0].1, LuaValue::Number(3.0));
        assert_eq!(pairs[1].1, LuaValue::String("ws".to_string()));
        assert_eq!(pairs[2].1, LuaValue::Bool(true));
    }
}
