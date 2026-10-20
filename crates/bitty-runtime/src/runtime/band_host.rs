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

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::band_slots::BandEdge;
use super::{BandContent, ChromeBands, Runtime};
use bitty_lua::host::LuaValue;
use bitty_lua::ui::{
    ClickArg, ClickCommand, UI_BUTTON_ARG, UI_BUTTON_LEFT, UI_BUTTON_MIDDLE, UI_BUTTON_RIGHT,
    UI_REGION_ARG, UI_SCROLL_DELTA_ARG, UiNode, UiRegion, UiSlot,
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
/// [`Runtime::queue_band_scroll`]), so the queue holds one frame of scrolls
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
    /// Per-item hit region effective for the span, if declared (issue #2248).
    ///
    /// One identity per span: the click binding's region wins, else the
    /// scroll binding's. Parser validation guarantees both agree when both
    /// are present. `None` keeps the legacy span-only click target.
    pub region: Option<UiRegion>,
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
    flatten_into(
        root, &mut text, &mut runs, None, None, None, None, None, false,
    );
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
    region: Option<UiRegion>,
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
            let on_click = leaf_click.clone().or(on_click);
            let on_scroll = leaf_scroll.clone().or(on_scroll);
            let region = on_click
                .as_ref()
                .and_then(|click| click.region.clone())
                .or_else(|| on_scroll.as_ref().and_then(|scroll| scroll.region.clone()))
                .or(region);
            runs.push(BandRun {
                start_col: start,
                end_col: display_cells(text),
                start_char,
                end_char: text.chars().count(),
                fg: leaf_fg.clone().or(fg),
                bg: leaf_bg.clone().or(bg),
                bold: leaf_bold.unwrap_or(bold),
                on_click,
                on_scroll,
                region,
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
            let region = on_click
                .as_ref()
                .and_then(|click| click.region.clone())
                .or_else(|| on_scroll.as_ref().and_then(|scroll| scroll.region.clone()))
                .or(region);
            for child in children {
                flatten_into(
                    child,
                    text,
                    runs,
                    fg.clone(),
                    bg.clone(),
                    on_click.clone(),
                    on_scroll.clone(),
                    region.clone(),
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

/// One bounded per-item hit region emitted by a band (issue #2248).
///
/// The rectangle is one row high and spans display cells `[start_col,
/// end_col)` inside the band, already clipped to the painted window width.
/// Core derives it from the same flattened runs the paint path uses, so
/// there is no second geometry source. The plugin never supplies
/// coordinates: it declares an identity beside a binding, and the host owns
/// the rect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandItemRegion {
    /// Owning plugin id (the band owner).
    pub plugin_id: String,
    /// Edge owning the band.
    pub edge: BandEdge,
    /// Visible index on the edge (hidden bands take no index).
    pub visible_index: usize,
    /// Window row the band paints.
    pub row: u16,
    /// Declared region identity (`id` + `kind`).
    pub region: UiRegion,
    /// First display-cell column of the region (inclusive, clipped).
    pub start_col: usize,
    /// One past the last display-cell column (exclusive, clipped).
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

/// Derives the bounded region table for one band (issue #2248).
///
/// Walks the flattened runs in declaration order, keeps spans that declare a
/// region, clips them to `window_width` display cells, and merges adjacent
/// spans with the same identity into one rect (a container region covering
/// several leaves is one item, not one item per leaf). Adjacent but
/// *different* regions stay separate. The result is sorted by
/// `start_col` by construction. More than [`BAND_MAX_REGIONS`] distinct
/// regions refuse the whole table.
pub fn band_regions_for(
    band: &BandContent,
    edge: BandEdge,
    visible_index: usize,
    row: u16,
    window_width: usize,
) -> Result<Vec<BandItemRegion>, BandRegionError> {
    let (_, runs) = flatten_band_runs(&band.root);
    let mut regions: Vec<BandItemRegion> = Vec::new();
    for run in runs {
        let Some(region) = run.region else {
            continue;
        };
        let start = run.start_col.min(window_width);
        let end = run.end_col.min(window_width);
        if start >= end {
            // Entirely clipped past the painted bounds.
            continue;
        }
        if let Some(back) = regions.last_mut() {
            if back.region == region && back.end_col == start {
                back.end_col = end;
                continue;
            }
        }
        if regions.len() >= BAND_MAX_REGIONS {
            return Err(BandRegionError::Overflow {
                count: regions.len().saturating_add(1),
                max: BAND_MAX_REGIONS,
            });
        }
        regions.push(BandItemRegion {
            plugin_id: band.plugin_id.clone(),
            edge,
            visible_index,
            row,
            region,
            start_col: start,
            end_col: end,
            version: band.version,
        });
    }
    Ok(regions)
}

/// Resolves a display-cell column to exactly one emitted region (issue #2248).
///
/// Later registration wins when regions overlap ("topmost/last-registered"):
/// emitted tables from [`band_regions_for`] are disjoint by construction, so
/// the tie-break is a documented invariant for any future overlapping
/// source rather than a silent first-match. A column outside every region
/// resolves to `None` (unclaimed chrome).
#[must_use]
pub fn resolve_item_region(regions: &[BandItemRegion], col: usize) -> Option<&BandItemRegion> {
    regions
        .iter()
        .rev()
        .find(|region| col >= region.start_col && col < region.end_col)
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
    /// Painted window row at capture time.
    pub row: u16,
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
    /// Executable origin for painted bands; `None` for hidden or vertical
    /// bands, which are never clickable.
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
/// Pure over one band: finds the run owning `char_idx` (display-cell
/// column) and splits its command against `owner`. Pure for headless tests;
/// geometry (which band owns the row) lives on [`Runtime::plugin_band_hit`].
#[must_use]
pub fn resolve_click_in_band(
    owner: &str,
    runs: &[BandRun],
    char_idx: usize,
    button: BandButton,
) -> BandColumnOutcome {
    let Some(run) = find_run_index(runs, char_idx).and_then(|index| runs.get(index)) else {
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
pub fn find_run_index(runs: &[BandRun], char_idx: usize) -> Option<usize> {
    runs.iter()
        .position(|run| char_idx >= run.start_col && char_idx < run.end_col)
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
    char_idx: usize,
    button: BandButton,
) -> BandColumnOutcome {
    let Some(run) = runs.iter().find(|run| {
        char_idx >= run.start_col && char_idx < run.end_col && run.region.as_ref() == Some(region)
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
    char_idx: usize,
) -> BandScrollOutcome {
    let Some(run) = runs.iter().find(|run| {
        char_idx >= run.start_col && char_idx < run.end_col && run.region.as_ref() == Some(region)
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
        region: run.region.clone(),
        origin: None,
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
    /// Display-cell column inside the band row (the region rect unit,
    /// issue #2248).
    pub col: usize,
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
    /// bands above the content floor, or for a vertical edge (this build has
    /// no vertical band host, so mounts to `left`/`right` fail closed and no
    /// vertical band can exist).
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
        let col = usize::try_from(cell_col).unwrap_or(usize::MAX);
        let char_idx = cell_col_to_char_idx(&text, col);
        Some(BandHit {
            edge: row_hit.edge,
            visible_index: row_hit.visible_index,
            row: row_hit.row,
            char_idx,
            col,
        })
    }

    /// Emits the bounded per-item region table for one visible band
    /// (issue #2248).
    ///
    /// `edge`/`visible_index` address the band in the same stacking order
    /// paint and hit-test use; `row` comes from [`Self::visible_band_row`].
    /// Rectangles are display-cell columns clipped to the painted window
    /// width. Regions are derived on demand from the current tree, so a
    /// remount or reflow can never expose stale geometry: the next call
    /// reads the new tree. A band above [`BAND_MAX_REGIONS`] returns
    /// [`BandRegionError::Overflow`] and routes nothing (fail-closed). No
    /// plugin VM is entered; this is a pure data walk.
    ///
    /// # Errors
    ///
    /// [`BandRegionError::MissingBand`] when no visible band occupies the
    /// index, or [`BandRegionError::Overflow`] for an over-cap band.
    pub fn band_item_regions(
        &self,
        edge: BandEdge,
        visible_index: usize,
    ) -> Result<Vec<BandItemRegion>, BandRegionError> {
        let row = self
            .visible_band_row(edge, visible_index)
            .ok_or(BandRegionError::MissingBand)?;
        let bands = self.visible_edge_bands(edge);
        let band = bands
            .get(visible_index)
            .ok_or(BandRegionError::MissingBand)?;
        band_regions_for(
            band,
            edge,
            visible_index,
            row,
            usize::from(self.window_cells().width),
        )
    }

    /// Routes a press on a plugin band row to chrome (CTX-0946 C1,
    /// issue #2248).
    ///
    /// Returns `true` (consume the press: no focus move, no selection, no
    /// capture report, and no clipboard paste) when the last-known cursor
    /// sits anywhere on a painted band row — including the unclaimed
    /// trailing region past the text — arming the one-shot
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
        let Some(row_hit) = self.plugin_band_row_hit(pos) else {
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
        if self.band_region_overflow(row_hit.edge, row_hit.visible_index) {
            self.band_stats.region_overflows = self.band_stats.region_overflows.saturating_add(1);
        }
        self.band_press_target = Some(self.band_target_at(pos, &row_hit, button));
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
    /// in [`Self::plugin_band_hit`], never the saturating tail of
    /// [`cell_col_to_char_idx`]). A declared region resolves to that item;
    /// a bare interactive span keeps the legacy span target.
    fn band_target_at(
        &self,
        pos: CursorPosition,
        row_hit: &BandRowHit,
        button: BandButton,
    ) -> PressedBandTarget {
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
        // Issue #2248: a declared region owns its rect. Overflow refuses the
        // whole band (fail-closed) rather than routing a partial table.
        let regions = match self.band_item_regions(row_hit.edge, row_hit.visible_index) {
            Ok(regions) => regions,
            Err(BandRegionError::MissingBand) => return PressedBandTarget::NoClaim,
            Err(BandRegionError::Overflow { .. }) => return PressedBandTarget::Denied,
        };
        let declared = resolve_item_region(&regions, hit.col).map(|item| item.region.clone());
        let outcome = match declared.as_ref() {
            Some(region) => resolve_click_for_region(&owner, &runs, region, hit.char_idx, button),
            None => resolve_click_in_band(&owner, &runs, hit.char_idx, button),
        };
        match outcome {
            BandColumnOutcome::Hit(request) => {
                let key = match request.region.clone() {
                    Some(region) => BandTargetKey::Region(region),
                    None => {
                        let Some(run) = find_run_index(&runs, hit.char_idx) else {
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
                    .published_band_origin(row_hit.edge, row_hit.visible_index)
                    .cloned();
                PressedBandTarget::Hit {
                    edge: row_hit.edge,
                    visible_index: row_hit.visible_index,
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
    /// The origin must match the live band identity *and* the live geometry;
    /// a replacement mount, version reset, edge move, or resize therefore
    /// refuses the gesture instead of retargeting it.
    fn published_band_origin(&self, edge: BandEdge, visible_index: usize) -> Option<&BandOrigin> {
        let index = u16::try_from(visible_index).ok()?;
        let row = self.visible_band_row(edge, visible_index)?;
        let bands = self.visible_edge_bands(edge);
        let band = bands.get(visible_index)?;
        self.published_band_origins.iter().find(|origin| {
            origin.edge == edge
                && origin.visible_index == index
                && origin.row == row
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
    /// Painted bands carry their pending [`BandOrigin`]; hidden and vertical
    /// bands carry `None` (they are never clickable) but still participate in
    /// damage identity through plugin id, slot, version, and edge.
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
        self.chrome_bands = bands;
        self.rebuild_pending_band_origins(mounts);
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
                        if let Some(row) = self.visible_band_row(edge, usize::from(visible_index)) {
                            pending.push(BandOrigin {
                                plugin_id,
                                generation,
                                handle,
                                slot,
                                version,
                                edge,
                                visible_index,
                                row,
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
        let Some(row_hit) = self.plugin_band_row_hit(pos) else {
            self.band_stats.clicks_unclaimed = self.band_stats.clicks_unclaimed.saturating_add(1);
            return true;
        };
        let released = self.band_target_at(pos, &row_hit, button);
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
        // Issue #2248: a declared region owns its rect; an over-cap band
        // refuses the wheel (fail-closed) instead of routing a partial table.
        let regions = match self.band_item_regions(row_hit.edge, row_hit.visible_index) {
            Ok(regions) => regions,
            Err(BandRegionError::MissingBand) => return ScrollTargetOutcome::Unclaimed,
            Err(BandRegionError::Overflow { .. }) => return ScrollTargetOutcome::Overflow,
        };
        let declared = resolve_item_region(&regions, hit.col).map(|item| item.region.clone());
        let outcome = match declared.as_ref() {
            Some(region) => resolve_scroll_for_region(&owner, &runs, region, hit.char_idx),
            None => resolve_scroll_in_band(&owner, &runs, hit.char_idx),
        };
        match outcome {
            BandScrollOutcome::Hit(mut target) => {
                target.origin = self
                    .published_band_origin(row_hit.edge, row_hit.visible_index)
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
                region: None,
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
                region: None,
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
            region: None,
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
            region: None,
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
                    region: None,
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
}
