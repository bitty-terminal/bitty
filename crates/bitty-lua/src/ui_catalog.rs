//! Extension component catalog (candidate catalog surface; no consumer yet).
//!
//! This module is the headless implementation slice of the candidate
//! `bitty-terminal-docs/specifications/extension-component-catalog-candidate.md`
//! record. It defines five typed descriptors for candidate extension building
//! blocks and composes them from the **accepted** Plugin API v1 node set only:
//! `Text`, `Row`, `Column`, and `List` (`crate::ui::UI_V1_NODE_KINDS`).
//!
//! Nothing here is an accepted contract, a plugin-facing API, a compatibility
//! promise, or a shipped extension capability. **No production consumer
//! exists:** no host path, band host, overlay path, bridge, render loop, Lua
//! surface, or plugin installs, composes, or renders these descriptors, and no
//! extension can opt in today. The descriptors are host-side Rust data; no Lua
//! value, VM, callback, renderer handle, capability, coordinate, or command
//! binding crosses this module.
//!
//! # Catalog entries
//!
//! | Entry                     | Shape                                                                  |
//! | ------------------------- | ---------------------------------------------------------------------- |
//! | `band`                    | `Row` of three region `Row`s (`left`, `center`, `right`)               |
//! | `pill-row`                | `Row` of bracketed `Text` pills                                        |
//! | `tooltip-detail-fallback` | one plain `Text` detail line                                           |
//! | `modal-sheet`             | `Column` of single-text `Row`s (title first)                           |
//! | `status-strip`            | `Row` of three region `Row`s, like `band`                              |
//!
//! Region separators are single spaces emitted only between two non-empty
//! regions; an empty leading or trailing region adds no edge blank.
//!
//! `band` and `status-strip` share a shape on purpose: they are separate
//! declarations with separate contracts (edge-band regions versus status
//! modules) that may diverge later. A composed tree carries no entry tag, so
//! [`CatalogBlock::from_scene`] takes the caller's expected entry; the entry
//! id is part of the declaration, never inferred from the tree. The two
//! entries are separated by contract semantics, not by shape, and are
//! interchangeable only when their descriptors are identical (presentation
//! data with no authority either way). A successor that needs them
//! distinguishable must add a tag to this candidate shape.
//!
//! # Composition rules
//!
//! - **Accepted primitives only.** Every emitted node is `Text`, `Row`, or
//!   `Column`. `Block`, `Image`, `CodeBlock`, `Table`, `Rule`, and unknown
//!   kinds are excluded exactly as the accepted v1 surface excludes them.
//! - **Deterministic order.** Declared order is render order: regions are
//!   `left` then `center` then `right`; segments, pills, and sheet rows keep
//!   declaration order; duplicates are preserved; nothing sorts, deduplicates,
//!   or evicts.
//! - **No authority.** No `fg`/`bg` token, no `on_click`/`on_scroll`, no
//!   command, no capability, no coordinate, no focus, and no modality. Bold is
//!   emitted only as redundant emphasis (active pill, emphasised segment,
//!   sheet title), never as the sole carrier of a state.
//! - **No style hooks yet.** The candidate Scene Style Primitives record is
//!   not accepted and has no consumer here; the catalog emits no style field
//!   and reserves hooks for a successor acceptance.
//! - **Cold path, no consumer.** Composition is a pure function over bounded
//!   data. It does not construct or call a Lua VM and is not wired into PTY,
//!   input, damage, or render loops; nothing in production calls it yet.
//!
//! # Caps (candidate; inclusive)
//!
//! | Constant                        | Value              | Anchor                                                        |
//! | ------------------------------- | ------------------ | ------------------------------------------------------------- |
//! | [`CATALOG_MAX_SEGMENTS`]        | `64`               | chrome-band candidate `BAND_MAX_REGIONS` (`64`)               |
//! | [`CATALOG_MAX_PILLS`]           | `64`               | same per-contribution item bound                              |
//! | [`CATALOG_MAX_ROWS`]            | `64`               | same per-contribution item bound                              |
//! | [`CATALOG_MAX_SEGMENT_BYTES`]   | `64`               | status-system draft module text (`64`)                        |
//! | [`CATALOG_MAX_PILL_LABEL_BYTES`] | `32`               | bar plugin workspace pill name bound (`32`)                   |
//! | [`CATALOG_MAX_DETAIL_BYTES`]    | `128`              | overlay/tooltip text bound (`128`)                            |
//! | [`CATALOG_MAX_TEXT_BYTES`]      | `256 KiB`          | accepted `SCN-3` / [`UI_MAX_TEXT_BYTES`]                      |
//! | [`CATALOG_MAX_NODES`]           | `2048`             | accepted `SCN-1` / [`UI_MAX_NODES`]                           |
//! | [`CATALOG_MAX_DEPTH`]           | `16`               | accepted v1 bridge budget / [`UI_MAX_DEPTH`]                  |
//!
//! Overflow is a typed rejection, never a clamp, truncation, or silent
//! omission. A declaration with no content is rejected too: a host may never
//! "ignore" an empty or unknown block and present a partial surface.
//!
//! # Cost
//!
//! Composition is `O(items)` time and allocates one bounded node tree; the
//! aggregate caps above bound both. Shape recognition walks the supplied tree
//! iteratively and stops at the first cap breach, so it is `O(min(n, caps))`
//! and cannot recurse without bound. No clock, randomness, hash iteration, or
//! platform state participates.

use std::fmt;

use crate::ui::{UI_MAX_DEPTH, UI_MAX_NODES, UI_MAX_TEXT_BYTES, UiNode};

/// Maximum segments across the three regions of one `band`/`status-strip`
/// declaration (candidate; anchored to the chrome-band candidate's
/// `BAND_MAX_REGIONS = 64`).
pub const CATALOG_MAX_SEGMENTS: usize = 64;

/// Maximum pills in one `pill-row` declaration (candidate; same
/// per-contribution item bound as [`CATALOG_MAX_SEGMENTS`]).
pub const CATALOG_MAX_PILLS: usize = 64;

/// Maximum body rows in one `modal-sheet` declaration (candidate; same
/// per-contribution item bound as [`CATALOG_MAX_SEGMENTS`]).
pub const CATALOG_MAX_ROWS: usize = 64;

/// Maximum bytes in one `band`/`status-strip` segment (candidate; anchored to
/// the status-system draft module text bound of `64`).
pub const CATALOG_MAX_SEGMENT_BYTES: usize = 64;

/// Maximum bytes in one pill label (candidate; anchored to the bar plugin's
/// workspace pill-name bound of `32`).
pub const CATALOG_MAX_PILL_LABEL_BYTES: usize = 32;

/// Maximum bytes in a detail fallback line, a modal sheet title, or a modal
/// sheet body row (candidate; anchored to the overlay/tooltip text bound of
/// `128`).
pub const CATALOG_MAX_DETAIL_BYTES: usize = 128;

/// Maximum text bytes in one composed block (accepted `SCN-3`).
pub const CATALOG_MAX_TEXT_BYTES: usize = UI_MAX_TEXT_BYTES;

/// Maximum nodes in one composed block (accepted `SCN-1`).
pub const CATALOG_MAX_NODES: usize = UI_MAX_NODES;

/// Maximum depth of one composed block (accepted v1 bridge budget).
pub const CATALOG_MAX_DEPTH: usize = UI_MAX_DEPTH;

/// Separator emitted between adjacent segments or pills.
const ITEM_SEPARATOR: &str = " ";

/// Separator emitted between the three strip regions.
const REGION_SEPARATOR: &str = " ";

/// Opening bracket of a pill label (bar-plugin pill spelling).
const PILL_OPEN: &str = "[";

/// Closing bracket of a pill label (bar-plugin pill spelling).
const PILL_CLOSE: &str = "]";

/// Closed set of catalog entries (candidate, bitty#2114).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CatalogEntry {
    /// Edge-band row with `left`, `center`, and `right` regions.
    Band,
    /// Row of bracketed state pills.
    PillRow,
    /// Plain detail text used when no tooltip vehicle is available.
    TooltipDetailFallback,
    /// Titled column of rows for a sheet-shaped surface.
    ModalSheet,
    /// Status-module row with `left`, `center`, and `right` slots.
    StatusStrip,
}

impl CatalogEntry {
    /// Every entry in canonical declaration order.
    pub const ALL: [Self; 5] = [
        Self::Band,
        Self::PillRow,
        Self::TooltipDetailFallback,
        Self::ModalSheet,
        Self::StatusStrip,
    ];

    /// Canonical descriptor name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Band => "band",
            Self::PillRow => "pill-row",
            Self::TooltipDetailFallback => "tooltip-detail-fallback",
            Self::ModalSheet => "modal-sheet",
            Self::StatusStrip => "status-strip",
        }
    }

    /// Parse an exact, case-sensitive descriptor name from the closed set.
    ///
    /// Returns `None` for every name outside [`Self::ALL`]; there are no
    /// aliases or case folding.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|entry| entry.as_str() == name)
    }
}

impl fmt::Display for CatalogEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One text segment in a band or status strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Bounded segment text.
    pub text: String,
    /// Redundant visual emphasis (bold); never the only state carrier.
    pub emphasis: bool,
}

impl Segment {
    /// A plain segment.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            emphasis: false,
        }
    }

    /// An emphasised segment.
    #[must_use]
    pub fn emphasized(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            emphasis: true,
        }
    }
}

/// Three-region descriptor shared by `band` and `status-strip`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StripDescriptor {
    /// Leading region, rendered first.
    pub left: Vec<Segment>,
    /// Middle region, rendered second.
    pub center: Vec<Segment>,
    /// Trailing region, rendered last.
    pub right: Vec<Segment>,
}

/// One pill in a [`PillRowDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    /// Bounded label without the rendered brackets.
    pub label: String,
    /// Active state, rendered as redundant bold emphasis.
    pub active: bool,
}

impl Pill {
    /// An inactive pill.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            active: false,
        }
    }

    /// An active pill.
    #[must_use]
    pub fn active(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            active: true,
        }
    }
}

/// Row of bracketed pills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PillRowDescriptor {
    /// Pills in declaration order.
    pub pills: Vec<Pill>,
}

/// Plain detail line used when no tooltip vehicle is available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailFallbackDescriptor {
    /// Bounded detail text.
    pub text: String,
}

/// Titled sheet content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModalSheetDescriptor {
    /// Title, rendered first and emphasised.
    pub title: String,
    /// Body rows in declaration order.
    pub rows: Vec<String>,
}

/// One typed catalog declaration.
///
/// Descriptors are plain data and may be constructed directly; [`Self::compose`]
/// and [`Self::verify`] are the validation boundary, so an invalid descriptor
/// can exist as a value but can never compose or verify as a tree.
///
/// The entry id is part of the declaration: [`Self::from_scene`] requires the
/// expected entry so `band` and `status-strip` (which share a shape) cannot be
/// confused, and an unknown or mismatched declaration fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogBlock {
    /// Edge-band row.
    Band(StripDescriptor),
    /// Row of state pills.
    PillRow(PillRowDescriptor),
    /// Plain detail line.
    TooltipDetailFallback(DetailFallbackDescriptor),
    /// Titled sheet content.
    ModalSheet(ModalSheetDescriptor),
    /// Status-module row.
    StatusStrip(StripDescriptor),
}

impl CatalogBlock {
    /// The declaration's entry id.
    #[must_use]
    pub fn entry(&self) -> CatalogEntry {
        match self {
            Self::Band(_) => CatalogEntry::Band,
            Self::PillRow(_) => CatalogEntry::PillRow,
            Self::TooltipDetailFallback(_) => CatalogEntry::TooltipDetailFallback,
            Self::ModalSheet(_) => CatalogEntry::ModalSheet,
            Self::StatusStrip(_) => CatalogEntry::StatusStrip,
        }
    }

    /// Validate the declaration and compose its bounded node tree.
    ///
    /// # Errors
    ///
    /// Returns a typed [`CatalogError`] for an empty declaration, an
    /// over-cap item count or item length, a reserved pill delimiter, or a
    /// breach of the aggregate scene bounds. Nothing is clamped or omitted.
    pub fn compose(&self) -> Result<UiNode, CatalogError> {
        self.validate()?;
        let node = match self {
            Self::Band(strip) | Self::StatusStrip(strip) => compose_strip(strip),
            Self::PillRow(row) => compose_pill_row(row),
            Self::TooltipDetailFallback(detail) => text_node(&detail.text, false),
            Self::ModalSheet(sheet) => compose_modal_sheet(sheet),
        };
        check_scene_bounds(self.entry(), &node)?;
        Ok(node)
    }

    /// Parse a presented tree back into the declaration for `entry`.
    ///
    /// `entry` is the caller's declared entry and is never inferred: a tree
    /// carries no entry tag, and `band`/`status-strip` share a shape.
    /// Recognition is canonical and strict: only the exact shape
    /// [`Self::compose`] emits (including absent style tokens and bindings) is
    /// accepted. A tree that diverges — because a segment, pill, or row was
    /// removed, changed, or decorated — fails closed with
    /// [`CatalogError::ShapeMismatch`] or a cap error instead of being
    /// ignored.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::ShapeMismatch`] when `node` is not the canonical
    /// tree for `entry`, and the same typed cap/empty errors as [`Self::compose`].
    pub fn from_scene(entry: CatalogEntry, node: &UiNode) -> Result<Self, CatalogError> {
        check_scene_bounds(entry, node)?;
        let block = match entry {
            CatalogEntry::TooltipDetailFallback => {
                let Some(text) = plain_text(node, false) else {
                    return Err(CatalogError::ShapeMismatch { entry });
                };
                Self::TooltipDetailFallback(DetailFallbackDescriptor {
                    text: text.to_string(),
                })
            }
            CatalogEntry::PillRow => {
                let Some(row) = pill_row_from_scene(node) else {
                    return Err(CatalogError::ShapeMismatch { entry });
                };
                Self::PillRow(row)
            }
            CatalogEntry::ModalSheet => {
                let Some(sheet) = modal_sheet_from_scene(node) else {
                    return Err(CatalogError::ShapeMismatch { entry });
                };
                Self::ModalSheet(sheet)
            }
            CatalogEntry::Band | CatalogEntry::StatusStrip => {
                let Some(strip) = strip_from_scene(node) else {
                    return Err(CatalogError::ShapeMismatch { entry });
                };
                if entry == CatalogEntry::Band {
                    Self::Band(strip)
                } else {
                    Self::StatusStrip(strip)
                }
            }
        };
        block.validate()?;
        Ok(block)
    }

    /// Verify that `node` is exactly this declaration's canonical tree.
    ///
    /// This is the fail-closed admission check for a declared block: a host
    /// that retains a descriptor and later presents a tree must not accept a
    /// partially composed or silently ignored block.
    ///
    /// # Errors
    ///
    /// Returns the [`Self::from_scene`] error, or
    /// [`CatalogError::ShapeMismatch`] when the tree is a valid canonical shape
    /// for the same entry but represents a different declaration.
    pub fn verify(&self, node: &UiNode) -> Result<(), CatalogError> {
        let parsed = Self::from_scene(self.entry(), node)?;
        if &parsed == self {
            Ok(())
        } else {
            Err(CatalogError::ShapeMismatch {
                entry: self.entry(),
            })
        }
    }

    fn validate(&self) -> Result<(), CatalogError> {
        match self {
            Self::Band(strip) | Self::StatusStrip(strip) => validate_strip(self.entry(), strip),
            Self::PillRow(row) => validate_pill_row(row),
            Self::TooltipDetailFallback(detail) => {
                validate_item(self.entry(), 0, &detail.text, CATALOG_MAX_DETAIL_BYTES)
            }
            Self::ModalSheet(sheet) => validate_modal_sheet(sheet),
        }
    }
}

fn validate_strip(entry: CatalogEntry, strip: &StripDescriptor) -> Result<(), CatalogError> {
    let total = strip.left.len() + strip.center.len() + strip.right.len();
    if total == 0 {
        return Err(CatalogError::EmptyBlock { entry });
    }
    if total > CATALOG_MAX_SEGMENTS {
        return Err(CatalogError::TooManyItems {
            entry,
            found: total,
            cap: CATALOG_MAX_SEGMENTS,
        });
    }
    for (index, segment) in strip
        .left
        .iter()
        .chain(strip.center.iter())
        .chain(strip.right.iter())
        .enumerate()
    {
        validate_item(entry, index, &segment.text, CATALOG_MAX_SEGMENT_BYTES)?;
    }
    Ok(())
}

fn validate_pill_row(row: &PillRowDescriptor) -> Result<(), CatalogError> {
    let entry = CatalogEntry::PillRow;
    if row.pills.is_empty() {
        return Err(CatalogError::EmptyBlock { entry });
    }
    if row.pills.len() > CATALOG_MAX_PILLS {
        return Err(CatalogError::TooManyItems {
            entry,
            found: row.pills.len(),
            cap: CATALOG_MAX_PILLS,
        });
    }
    for (index, pill) in row.pills.iter().enumerate() {
        validate_item(entry, index, &pill.label, CATALOG_MAX_PILL_LABEL_BYTES)?;
        if pill.label.contains(PILL_OPEN) || pill.label.contains(PILL_CLOSE) {
            return Err(CatalogError::ReservedDelimiter { entry, index });
        }
    }
    Ok(())
}

fn validate_modal_sheet(sheet: &ModalSheetDescriptor) -> Result<(), CatalogError> {
    let entry = CatalogEntry::ModalSheet;
    validate_item(entry, 0, &sheet.title, CATALOG_MAX_DETAIL_BYTES)?;
    if sheet.rows.is_empty() {
        return Err(CatalogError::EmptyBlock { entry });
    }
    if sheet.rows.len() > CATALOG_MAX_ROWS {
        return Err(CatalogError::TooManyItems {
            entry,
            found: sheet.rows.len(),
            cap: CATALOG_MAX_ROWS,
        });
    }
    for (index, row) in sheet.rows.iter().enumerate() {
        validate_item(entry, index + 1, row, CATALOG_MAX_DETAIL_BYTES)?;
    }
    Ok(())
}

fn validate_item(
    entry: CatalogEntry,
    index: usize,
    text: &str,
    cap: usize,
) -> Result<(), CatalogError> {
    if text.is_empty() {
        return Err(CatalogError::EmptyItem { entry, index });
    }
    if text.len() > cap {
        return Err(CatalogError::ItemTooLong {
            entry,
            index,
            len: text.len(),
            cap,
        });
    }
    Ok(())
}

fn check_scene_bounds(entry: CatalogEntry, node: &UiNode) -> Result<(), CatalogError> {
    // Iterative and early-exiting: a supplied tree is untrusted input to
    // recognition, so neither recursion depth nor a wide child list may grow
    // past the caps before rejection.
    let mut pending = vec![(node, 1usize)];
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((current, depth)) = pending.pop() {
        nodes += 1;
        if nodes > CATALOG_MAX_NODES {
            return Err(CatalogError::TooManyNodes {
                found: nodes,
                cap: CATALOG_MAX_NODES,
            });
        }
        if depth > CATALOG_MAX_DEPTH {
            return Err(CatalogError::TooDeep {
                found: depth,
                cap: CATALOG_MAX_DEPTH,
            });
        }
        match current {
            UiNode::Text { text, .. } => {
                bytes = bytes.saturating_add(text.len());
                if bytes > CATALOG_MAX_TEXT_BYTES {
                    return Err(CatalogError::TextTooLong {
                        entry,
                        bytes,
                        cap: CATALOG_MAX_TEXT_BYTES,
                    });
                }
            }
            UiNode::Row { children, .. }
            | UiNode::Column { children, .. }
            | UiNode::List { children, .. } => {
                for child in children {
                    if nodes.saturating_add(pending.len()) >= CATALOG_MAX_NODES {
                        return Err(CatalogError::TooManyNodes {
                            found: CATALOG_MAX_NODES + 1,
                            cap: CATALOG_MAX_NODES,
                        });
                    }
                    let child_depth = depth + 1;
                    if child_depth > CATALOG_MAX_DEPTH {
                        return Err(CatalogError::TooDeep {
                            found: child_depth,
                            cap: CATALOG_MAX_DEPTH,
                        });
                    }
                    pending.push((child, child_depth));
                }
            }
        }
    }
    Ok(())
}

fn compose_strip(strip: &StripDescriptor) -> UiNode {
    let mut children = Vec::with_capacity(5);
    children.push(compose_group(&strip.left));
    if !strip.left.is_empty() && !strip.center.is_empty() {
        children.push(UiNode::text(REGION_SEPARATOR));
    }
    children.push(compose_group(&strip.center));
    if !strip.center.is_empty() && !strip.right.is_empty() {
        children.push(UiNode::text(REGION_SEPARATOR));
    }
    children.push(compose_group(&strip.right));
    UiNode::row(children)
}

fn compose_group(segments: &[Segment]) -> UiNode {
    let mut children = Vec::with_capacity(segments.len().saturating_mul(2).saturating_sub(1));
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            children.push(UiNode::text(ITEM_SEPARATOR));
        }
        children.push(text_node(&segment.text, segment.emphasis));
    }
    UiNode::row(children)
}

fn compose_pill_row(row: &PillRowDescriptor) -> UiNode {
    let mut children = Vec::with_capacity(row.pills.len().saturating_mul(2).saturating_sub(1));
    for (index, pill) in row.pills.iter().enumerate() {
        if index > 0 {
            children.push(UiNode::text(ITEM_SEPARATOR));
        }
        children.push(text_node(
            &format!("{PILL_OPEN}{}{PILL_CLOSE}", pill.label),
            pill.active,
        ));
    }
    UiNode::row(children)
}

fn compose_modal_sheet(sheet: &ModalSheetDescriptor) -> UiNode {
    let mut rows = Vec::with_capacity(sheet.rows.len() + 1);
    rows.push(UiNode::row(vec![text_node(&sheet.title, true)]));
    for row in &sheet.rows {
        rows.push(UiNode::row(vec![text_node(row, false)]));
    }
    UiNode::column(rows)
}

fn text_node(text: &str, bold: bool) -> UiNode {
    UiNode::Text {
        text: text.to_string(),
        fg: None,
        bg: None,
        bold: if bold { Some(true) } else { None },
        on_click: None,
        on_scroll: None,
    }
}

/// A plain, unbound text leaf with the expected canonical bold state.
///
/// Canonical means exactly `bold = Some(true)` when `bold` is expected and
/// exactly `bold = None` otherwise: an explicit `Some(false)` is not the tree
/// [`text_node`] emits and is rejected rather than normalized.
fn plain_text(node: &UiNode, bold: bool) -> Option<&str> {
    match node {
        UiNode::Text {
            text,
            fg,
            bg,
            bold: node_bold,
            on_click,
            on_scroll,
        } if fg.is_none()
            && bg.is_none()
            && on_click.is_none()
            && on_scroll.is_none()
            && if bold {
                *node_bold == Some(true)
            } else {
                node_bold.is_none()
            } =>
        {
            Some(text.as_str())
        }
        _ => None,
    }
}

fn is_plain_container(node: &UiNode) -> Option<&[UiNode]> {
    match node {
        UiNode::Row {
            children,
            fg,
            bg,
            bold,
            on_click,
            on_scroll,
        }
        | UiNode::Column {
            children,
            fg,
            bg,
            bold,
            on_click,
            on_scroll,
        }
        | UiNode::List {
            children,
            fg,
            bg,
            bold,
            on_click,
            on_scroll,
        } if fg.is_none()
            && bg.is_none()
            && bold.is_none()
            && on_click.is_none()
            && on_scroll.is_none() =>
        {
            Some(children.as_slice())
        }
        _ => None,
    }
}

fn is_unstyled_row(node: &UiNode) -> Option<&[UiNode]> {
    match node {
        UiNode::Row { .. } => is_plain_container(node),
        _ => None,
    }
}

fn is_unstyled_column(node: &UiNode) -> Option<&[UiNode]> {
    match node {
        UiNode::Column { .. } => is_plain_container(node),
        _ => None,
    }
}

fn strip_from_scene(node: &UiNode) -> Option<StripDescriptor> {
    let children = is_unstyled_row(node)?;
    let mut groups: Vec<Vec<Segment>> = Vec::with_capacity(3);
    let mut last_was_separator = false;
    for child in children {
        if let Some(group) = group_from_scene(child) {
            if last_was_separator && group.is_empty() {
                return None;
            }
            if !last_was_separator
                && groups
                    .last()
                    .is_some_and(|previous: &Vec<Segment>| !previous.is_empty())
                && !group.is_empty()
            {
                return None;
            }
            groups.push(group);
            if groups.len() > 3 {
                return None;
            }
            last_was_separator = false;
        } else if plain_text(child, false) == Some(REGION_SEPARATOR) {
            if last_was_separator || groups.is_empty() {
                return None;
            }
            if groups.last().is_some_and(Vec::is_empty) {
                return None;
            }
            last_was_separator = true;
        } else {
            return None;
        }
    }
    if last_was_separator || groups.len() != 3 {
        return None;
    }
    let right = groups.pop()?;
    let center = groups.pop()?;
    let left = groups.pop()?;
    Some(StripDescriptor {
        left,
        center,
        right,
    })
}

fn group_from_scene(node: &UiNode) -> Option<Vec<Segment>> {
    let children = is_unstyled_row(node)?;
    if children.is_empty() {
        return Some(Vec::new());
    }
    if children.len() % 2 == 0 {
        return None;
    }
    let mut segments = Vec::with_capacity(children.len().div_ceil(2));
    for (index, child) in children.iter().enumerate() {
        if index % 2 == 1 {
            if plain_text(child, false) != Some(ITEM_SEPARATOR) {
                return None;
            }
            continue;
        }
        let emphasis = plain_text(child, true);
        let text = emphasis.or_else(|| plain_text(child, false))?;
        segments.push(Segment {
            text: text.to_string(),
            emphasis: emphasis.is_some(),
        });
    }
    Some(segments)
}

fn pill_row_from_scene(node: &UiNode) -> Option<PillRowDescriptor> {
    let children = is_unstyled_row(node)?;
    if children.is_empty() || children.len() % 2 == 0 {
        return None;
    }
    let mut pills = Vec::with_capacity(children.len().div_ceil(2));
    for (index, child) in children.iter().enumerate() {
        if index % 2 == 1 {
            if plain_text(child, false) != Some(ITEM_SEPARATOR) {
                return None;
            }
            continue;
        }
        let active = plain_text(child, true);
        let label = active.or_else(|| plain_text(child, false))?;
        let label = label.strip_prefix(PILL_OPEN)?.strip_suffix(PILL_CLOSE)?;
        if label.contains(PILL_OPEN) || label.contains(PILL_CLOSE) {
            return None;
        }
        pills.push(Pill {
            label: label.to_string(),
            active: active.is_some(),
        });
    }
    Some(PillRowDescriptor { pills })
}

fn modal_sheet_from_scene(node: &UiNode) -> Option<ModalSheetDescriptor> {
    let children = is_unstyled_column(node)?;
    if children.is_empty() {
        return None;
    }
    let mut title = None;
    let mut rows = Vec::with_capacity(children.len().saturating_sub(1));
    for (index, child) in children.iter().enumerate() {
        let row = is_unstyled_row(child)?;
        if row.len() != 1 {
            return None;
        }
        if index == 0 {
            title = Some(plain_text(&row[0], true)?.to_string());
        } else {
            rows.push(plain_text(&row[0], false)?.to_string());
        }
    }
    Some(ModalSheetDescriptor {
        title: title?,
        rows,
    })
}

/// Failure to validate, compose, or recognize a catalog declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogError {
    /// The declaration declares the entry but no content.
    EmptyBlock {
        /// Declared entry.
        entry: CatalogEntry,
    },
    /// One item is empty.
    EmptyItem {
        /// Declared entry.
        entry: CatalogEntry,
        /// Zero-based item index in declaration order.
        index: usize,
    },
    /// The declaration holds more items than the entry cap.
    TooManyItems {
        /// Declared entry.
        entry: CatalogEntry,
        /// Items counted across the declaration.
        found: usize,
        /// The inclusive cap that was exceeded.
        cap: usize,
    },
    /// One item exceeds its byte cap.
    ItemTooLong {
        /// Declared entry.
        entry: CatalogEntry,
        /// Zero-based item index in declaration order.
        index: usize,
        /// Bytes in the rejected item.
        len: usize,
        /// The inclusive cap that was exceeded.
        cap: usize,
    },
    /// The tree exceeds the accepted block text budget.
    TextTooLong {
        /// Declared entry.
        entry: CatalogEntry,
        /// Text bytes counted before rejection.
        bytes: usize,
        /// The inclusive cap that was exceeded.
        cap: usize,
    },
    /// The tree exceeds the accepted block node budget.
    TooManyNodes {
        /// Nodes counted before rejection (bounded at `cap + 1` by recognition).
        found: usize,
        /// The inclusive cap that was exceeded.
        cap: usize,
    },
    /// The tree exceeds the accepted block depth budget.
    TooDeep {
        /// Depth at rejection (bounded at `cap + 1` by recognition).
        found: usize,
        /// The inclusive cap that was exceeded.
        cap: usize,
    },
    /// A pill label contains a rendered bracket delimiter.
    ReservedDelimiter {
        /// Declared entry.
        entry: CatalogEntry,
        /// Zero-based pill index in declaration order.
        index: usize,
    },
    /// The presented tree is not the canonical tree for the declared entry.
    ShapeMismatch {
        /// Declared entry.
        entry: CatalogEntry,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBlock { entry } => write!(f, "{entry} block has no content"),
            Self::EmptyItem { entry, index } => {
                write!(f, "{entry} item {index} is empty")
            }
            Self::TooManyItems { entry, found, cap } => {
                write!(f, "{entry} has {found} items, exceeding cap {cap}")
            }
            Self::ItemTooLong {
                entry,
                index,
                len,
                cap,
            } => write!(
                f,
                "{entry} item {index} is {len} bytes, exceeding cap {cap}"
            ),
            Self::TextTooLong { entry, bytes, cap } => {
                write!(f, "{entry} text is {bytes} bytes, exceeding cap {cap}")
            }
            Self::TooManyNodes { found, cap } => {
                write!(f, "composed block has {found} nodes, exceeding cap {cap}")
            }
            Self::TooDeep { found, cap } => {
                write!(f, "composed block depth {found} exceeds cap {cap}")
            }
            Self::ReservedDelimiter { entry, index } => {
                write!(
                    f,
                    "{entry} pill {index} contains a reserved bracket delimiter"
                )
            }
            Self::ShapeMismatch { entry } => {
                write!(f, "presented tree is not the canonical {entry} shape")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UI_V1_NODE_KINDS;

    fn band(left: &[&str], center: &[&str], right: &[&str]) -> StripDescriptor {
        StripDescriptor {
            left: left.iter().map(|text| Segment::new(*text)).collect(),
            center: center.iter().map(|text| Segment::new(*text)).collect(),
            right: right.iter().map(|text| Segment::new(*text)).collect(),
        }
    }

    fn pills(labels: &[&str]) -> PillRowDescriptor {
        PillRowDescriptor {
            pills: labels.iter().map(|label| Pill::new(*label)).collect(),
        }
    }

    fn flatten(node: &UiNode) -> String {
        match node {
            UiNode::Text { text, .. } => text.clone(),
            UiNode::Row { children, .. }
            | UiNode::Column { children, .. }
            | UiNode::List { children, .. } => children.iter().map(flatten).collect(),
        }
    }

    /// Every emitted text leaf in tree order, separators included.
    fn emitted_texts(node: &UiNode) -> Vec<String> {
        match node {
            UiNode::Text { text, .. } => vec![text.clone()],
            UiNode::Row { children, .. }
            | UiNode::Column { children, .. }
            | UiNode::List { children, .. } => children.iter().flat_map(emitted_texts).collect(),
        }
    }

    fn encode(node: &UiNode) -> String {
        fn option(out: &mut String, value: &Option<String>) {
            match value {
                Some(text) => {
                    out.push('S');
                    out.push_str(&text.len().to_string());
                    out.push(':');
                    out.push_str(text);
                }
                None => out.push('N'),
            }
        }
        fn walk(node: &UiNode, out: &mut String) {
            match node {
                UiNode::Text {
                    text,
                    fg,
                    bg,
                    bold,
                    on_click,
                    on_scroll,
                } => {
                    out.push_str("T(");
                    out.push_str(if bold.unwrap_or(false) { "b" } else { "-" });
                    out.push(',');
                    option(out, fg);
                    out.push(',');
                    option(out, bg);
                    out.push(',');
                    out.push(if on_click.is_some() { 'c' } else { '-' });
                    out.push(',');
                    out.push(if on_scroll.is_some() { 's' } else { '-' });
                    out.push(',');
                    out.push_str(&text.len().to_string());
                    out.push(':');
                    out.push_str(text);
                    out.push(')');
                }
                UiNode::Row { children, .. } => container('R', children, out),
                UiNode::Column { children, .. } => container('C', children, out),
                UiNode::List { children, .. } => container('L', children, out),
            }
        }
        fn container(kind: char, children: &[UiNode], out: &mut String) {
            out.push(kind);
            out.push('[');
            for child in children {
                walk(child, out);
            }
            out.push(']');
        }
        let mut out = String::new();
        walk(node, &mut out);
        out
    }

    #[test]
    fn entry_names_round_trip_and_reject_aliases() {
        assert_eq!(CatalogEntry::ALL.len(), 5);
        for entry in CatalogEntry::ALL {
            assert_eq!(CatalogEntry::parse(entry.as_str()), Some(entry));
            assert!(entry.as_str().len() <= 32);
        }
        for bad in [
            "",
            "Band",
            "pill_row",
            "pill-row ",
            " tooltip-detail-fallback",
            "statusline",
            "tooltip",
            "modal",
        ] {
            assert_eq!(CatalogEntry::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn band_preserves_region_and_segment_order() {
        let block = CatalogBlock::Band(band(&["b", "a"], &["c"], &["d", "e"]));
        let node = block.compose().expect("bounded band");
        assert_eq!(flatten(&node), "b a c d e");
        let UiNode::Row { children, .. } = &node else {
            panic!("band root is a Row");
        };
        assert_eq!(children.len(), 5);
        assert_eq!(flatten(&children[0]), "b a");
        assert_eq!(flatten(&children[2]), "c");
        assert_eq!(flatten(&children[4]), "d e");
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &node),
            Ok(block)
        );
    }

    #[test]
    fn empty_regions_emit_no_edge_separators() {
        let block = CatalogBlock::Band(band(&[], &["cpu"], &[]));
        let node = block.compose().expect("center-only band");
        assert_eq!(flatten(&node), "cpu");
        let UiNode::Row { children, .. } = &node else {
            panic!("band root is a Row");
        };
        assert_eq!(children.len(), 3);
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &node),
            Ok(block)
        );

        let block = CatalogBlock::StatusStrip(band(&["a"], &["b"], &[]));
        let node = block.compose().expect("left-center strip");
        assert_eq!(flatten(&node), "a b");
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::StatusStrip, &node),
            Ok(block)
        );

        // A non-canonical edge separator is rejected, not ignored.
        let UiNode::Row { children, .. } = &node else {
            panic!("strip root is a Row");
        };
        let mut decorated = children.clone();
        decorated.push(UiNode::text(REGION_SEPARATOR));
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::StatusStrip, &UiNode::row(decorated)),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::StatusStrip,
            })
        );
    }

    #[test]
    fn status_strip_preserves_left_center_right_order() {
        let block = CatalogBlock::StatusStrip(band(&["ws"], &["cpu"], &["clock"]));
        let node = block.compose().expect("bounded status strip");
        assert_eq!(flatten(&node), "ws cpu clock");
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::StatusStrip, &node),
            Ok(block)
        );
        // The shape is shared and carries no tag: the caller's declared entry
        // selects the contract, and the tree alone cannot switch it.
        let as_band = CatalogBlock::from_scene(CatalogEntry::Band, &node).expect("shared strip");
        assert_eq!(as_band.entry(), CatalogEntry::Band);
        assert_eq!(
            as_band,
            CatalogBlock::Band(band(&["ws"], &["cpu"], &["clock"]))
        );
    }

    #[test]
    fn shuffled_inputs_compose_in_exact_declaration_order() {
        // Every declaration below is deliberately shuffled and every token is
        // unique: sorting, deduplicating, or reversing any collection changes
        // the exact emitted sequence. The assertions observe what `compose`
        // emitted (leaf texts and separators in tree order), not a sorted view
        // of the inputs.
        let left = ["zulu", "alpha", "mike"];
        let center = ["yankee", "bravo"];
        let right = ["xray", "delta", "echo"];
        let band_node = CatalogBlock::Band(band(&left, &center, &right))
            .compose()
            .expect("bounded band");
        assert_eq!(
            emitted_texts(&band_node),
            [
                "zulu", " ", "alpha", " ", "mike", " ", "yankee", " ", "bravo", " ", "xray", " ",
                "delta", " ", "echo",
            ]
        );
        assert_eq!(
            flatten(&band_node),
            "zulu alpha mike yankee bravo xray delta echo"
        );
        let mut sorted = left
            .into_iter()
            .chain(center)
            .chain(right)
            .collect::<Vec<_>>();
        sorted.sort_unstable();
        assert_ne!(flatten(&band_node), sorted.join(" "));

        let strip_node = CatalogBlock::StatusStrip(band(&right, &left, &center))
            .compose()
            .expect("bounded status strip");
        assert_eq!(
            emitted_texts(&strip_node),
            [
                "xray", " ", "delta", " ", "echo", " ", "zulu", " ", "alpha", " ", "mike", " ",
                "yankee", " ", "bravo",
            ]
        );

        let pill_node = CatalogBlock::PillRow(pills(&["zulu", "alpha", "mike", "bravo"]))
            .compose()
            .expect("bounded pill row");
        assert_eq!(
            emitted_texts(&pill_node),
            ["[zulu]", " ", "[alpha]", " ", "[mike]", " ", "[bravo]"]
        );

        let sheet_node = CatalogBlock::ModalSheet(ModalSheetDescriptor {
            title: "Title".to_string(),
            rows: vec!["zulu".to_string(), "alpha".to_string(), "mike".to_string()],
        })
        .compose()
        .expect("bounded sheet");
        assert_eq!(
            emitted_texts(&sheet_node),
            ["Title", "zulu", "alpha", "mike"]
        );
    }

    #[test]
    fn pill_row_preserves_order_duplicates_and_active_state() {
        let block = CatalogBlock::PillRow(PillRowDescriptor {
            pills: vec![Pill::new("b"), Pill::active("a"), Pill::new("a")],
        });
        let node = block.compose().expect("bounded pill row");
        assert_eq!(flatten(&node), "[b] [a] [a]");
        let UiNode::Row { children, .. } = &node else {
            panic!("pill row root is a Row");
        };
        assert_eq!(children.len(), 5);
        assert_eq!(plain_text(&children[0], false), Some("[b]"));
        assert_eq!(plain_text(&children[2], true), Some("[a]"));
        assert_eq!(plain_text(&children[4], false), Some("[a]"));
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::PillRow, &node),
            Ok(block)
        );
    }

    #[test]
    fn modal_sheet_renders_title_first() {
        let block = CatalogBlock::ModalSheet(ModalSheetDescriptor {
            title: "Confirm".to_string(),
            rows: vec!["first".to_string(), "second".to_string()],
        });
        let node = block.compose().expect("bounded sheet");
        assert_eq!(flatten(&node), "Confirmfirstsecond");
        let UiNode::Column { children, .. } = &node else {
            panic!("sheet root is a Column");
        };
        assert_eq!(children.len(), 3);
        assert!(plain_text(&single_text(&children[0]), true).is_some());
        assert!(plain_text(&single_text(&children[1]), false).is_some());
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::ModalSheet, &node),
            Ok(block)
        );
    }

    fn single_text(node: &UiNode) -> UiNode {
        let UiNode::Row { children, .. } = node else {
            panic!("sheet row is a Row");
        };
        assert_eq!(children.len(), 1);
        children[0].clone()
    }

    #[test]
    fn detail_fallback_is_one_plain_text_node() {
        let block = CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor {
            text: "no tooltip vehicle".to_string(),
        });
        let node = block.compose().expect("bounded detail");
        assert_eq!(plain_text(&node, false), Some("no tooltip vehicle"));
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::TooltipDetailFallback, &node),
            Ok(block)
        );
    }

    #[test]
    fn segment_count_cap_is_inclusive_across_regions() {
        let segments = |count: usize| -> Vec<Segment> {
            (0..count)
                .map(|index| Segment::new(format!("s{index}")))
                .collect()
        };
        // The cap counts every region, and exactly 64 split across all three
        // regions is accepted.
        let at_cap = CatalogBlock::Band(StripDescriptor {
            left: segments(22),
            center: segments(21),
            right: segments(21),
        });
        assert!(at_cap.compose().is_ok());

        // One segment past the cap is a typed rejection, never a clamp.
        let over_cap = CatalogBlock::StatusStrip(StripDescriptor {
            left: segments(22),
            center: segments(21),
            right: segments(22),
        });
        assert_eq!(
            over_cap.compose(),
            Err(CatalogError::TooManyItems {
                entry: CatalogEntry::StatusStrip,
                found: CATALOG_MAX_SEGMENTS + 1,
                cap: CATALOG_MAX_SEGMENTS,
            })
        );
    }

    #[test]
    fn pill_count_cap_is_inclusive() {
        let row = |count: usize| PillRowDescriptor {
            pills: (0..count)
                .map(|index| Pill::new(format!("p{index}")))
                .collect(),
        };
        assert!(
            CatalogBlock::PillRow(row(CATALOG_MAX_PILLS))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::PillRow(row(CATALOG_MAX_PILLS + 1)).compose(),
            Err(CatalogError::TooManyItems {
                entry: CatalogEntry::PillRow,
                found: CATALOG_MAX_PILLS + 1,
                cap: CATALOG_MAX_PILLS,
            })
        );
    }

    #[test]
    fn modal_sheet_row_count_cap_is_inclusive() {
        let sheet = |count: usize| ModalSheetDescriptor {
            title: "title".to_string(),
            rows: (0..count).map(|index| format!("r{index}")).collect(),
        };
        assert!(
            CatalogBlock::ModalSheet(sheet(CATALOG_MAX_ROWS))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::ModalSheet(sheet(CATALOG_MAX_ROWS + 1)).compose(),
            Err(CatalogError::TooManyItems {
                entry: CatalogEntry::ModalSheet,
                found: CATALOG_MAX_ROWS + 1,
                cap: CATALOG_MAX_ROWS,
            })
        );
    }

    #[test]
    fn segment_byte_cap_is_inclusive() {
        let strip = |text: String| StripDescriptor {
            left: vec![Segment::new(text)],
            ..StripDescriptor::default()
        };
        assert!(
            CatalogBlock::StatusStrip(strip("x".repeat(CATALOG_MAX_SEGMENT_BYTES)))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::StatusStrip(strip("x".repeat(CATALOG_MAX_SEGMENT_BYTES + 1))).compose(),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::StatusStrip,
                index: 0,
                len: CATALOG_MAX_SEGMENT_BYTES + 1,
                cap: CATALOG_MAX_SEGMENT_BYTES,
            })
        );
    }

    #[test]
    fn pill_label_byte_cap_is_inclusive() {
        let row = |len: usize| PillRowDescriptor {
            pills: vec![Pill::new("x".repeat(len))],
        };
        assert!(
            CatalogBlock::PillRow(row(CATALOG_MAX_PILL_LABEL_BYTES))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::PillRow(row(CATALOG_MAX_PILL_LABEL_BYTES + 1)).compose(),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::PillRow,
                index: 0,
                len: CATALOG_MAX_PILL_LABEL_BYTES + 1,
                cap: CATALOG_MAX_PILL_LABEL_BYTES,
            })
        );
    }

    #[test]
    fn detail_fallback_byte_cap_is_inclusive() {
        assert!(
            CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor {
                text: "x".repeat(CATALOG_MAX_DETAIL_BYTES),
            })
            .compose()
            .is_ok()
        );
        assert_eq!(
            CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor {
                text: "x".repeat(CATALOG_MAX_DETAIL_BYTES + 1),
            })
            .compose(),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::TooltipDetailFallback,
                index: 0,
                len: CATALOG_MAX_DETAIL_BYTES + 1,
                cap: CATALOG_MAX_DETAIL_BYTES,
            })
        );
    }

    #[test]
    fn modal_sheet_title_byte_cap_is_inclusive() {
        let sheet = |title: String| ModalSheetDescriptor {
            title,
            rows: vec!["row".to_string()],
        };
        assert!(
            CatalogBlock::ModalSheet(sheet("x".repeat(CATALOG_MAX_DETAIL_BYTES)))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::ModalSheet(sheet("x".repeat(CATALOG_MAX_DETAIL_BYTES + 1))).compose(),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::ModalSheet,
                index: 0,
                len: CATALOG_MAX_DETAIL_BYTES + 1,
                cap: CATALOG_MAX_DETAIL_BYTES,
            })
        );
    }

    #[test]
    fn modal_sheet_row_byte_cap_is_inclusive() {
        let sheet = |len: usize| ModalSheetDescriptor {
            title: "title".to_string(),
            rows: vec!["x".repeat(len)],
        };
        assert!(
            CatalogBlock::ModalSheet(sheet(CATALOG_MAX_DETAIL_BYTES))
                .compose()
                .is_ok()
        );
        assert_eq!(
            CatalogBlock::ModalSheet(sheet(CATALOG_MAX_DETAIL_BYTES + 1)).compose(),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::ModalSheet,
                index: 1,
                len: CATALOG_MAX_DETAIL_BYTES + 1,
                cap: CATALOG_MAX_DETAIL_BYTES,
            })
        );
    }

    #[test]
    fn nested_block_depth_cap_is_inclusive() {
        // 15 wrappers put the leaf exactly at `CATALOG_MAX_DEPTH`; the tree is
        // still outside the catalog shape, so the failure is a shape mismatch
        // and not a depth breach.
        let nested = |wrappers: usize| {
            let mut node = UiNode::text("x");
            for _ in 0..wrappers {
                node = UiNode::row(vec![node]);
            }
            node
        };
        let at_cap = nested(CATALOG_MAX_DEPTH - 1);
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &at_cap),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::Band,
            })
        );

        let over_cap = nested(CATALOG_MAX_DEPTH);
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &over_cap),
            Err(CatalogError::TooDeep {
                found: CATALOG_MAX_DEPTH + 1,
                cap: CATALOG_MAX_DEPTH,
            })
        );
    }

    #[test]
    fn nested_block_node_cap_is_inclusive() {
        // A flat tree with exactly `CATALOG_MAX_NODES` nodes passes the bound
        // check and fails later as a shape mismatch; one more node fails the
        // bound itself.
        let wide =
            |children: usize| UiNode::row((0..children).map(|_| UiNode::text("x")).collect());
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &wide(CATALOG_MAX_NODES - 1)),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::Band,
            })
        );
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &wide(CATALOG_MAX_NODES)),
            Err(CatalogError::TooManyNodes {
                found: CATALOG_MAX_NODES + 1,
                cap: CATALOG_MAX_NODES,
            })
        );
    }

    #[test]
    fn aggregate_text_cap_is_inclusive() {
        // One text leaf at exactly the aggregate budget passes the bound check;
        // its per-entry detail cap then rejects it as an item, proving the
        // aggregate cap did not fire at the boundary.
        let at_cap = UiNode::text("x".repeat(CATALOG_MAX_TEXT_BYTES));
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::TooltipDetailFallback, &at_cap),
            Err(CatalogError::ItemTooLong {
                entry: CatalogEntry::TooltipDetailFallback,
                index: 0,
                len: CATALOG_MAX_TEXT_BYTES,
                cap: CATALOG_MAX_DETAIL_BYTES,
            })
        );

        let over_cap = UiNode::text("x".repeat(CATALOG_MAX_TEXT_BYTES + 1));
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::TooltipDetailFallback, &over_cap),
            Err(CatalogError::TextTooLong {
                entry: CatalogEntry::TooltipDetailFallback,
                bytes: CATALOG_MAX_TEXT_BYTES + 1,
                cap: CATALOG_MAX_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn empty_declarations_and_items_fail_closed() {
        assert_eq!(
            CatalogBlock::Band(StripDescriptor::default()).compose(),
            Err(CatalogError::EmptyBlock {
                entry: CatalogEntry::Band,
            })
        );
        assert_eq!(
            CatalogBlock::StatusStrip(StripDescriptor::default()).compose(),
            Err(CatalogError::EmptyBlock {
                entry: CatalogEntry::StatusStrip,
            })
        );
        assert_eq!(
            CatalogBlock::PillRow(PillRowDescriptor::default()).compose(),
            Err(CatalogError::EmptyBlock {
                entry: CatalogEntry::PillRow,
            })
        );
        assert_eq!(
            CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor::default()).compose(),
            Err(CatalogError::EmptyItem {
                entry: CatalogEntry::TooltipDetailFallback,
                index: 0,
            })
        );
        assert_eq!(
            CatalogBlock::ModalSheet(ModalSheetDescriptor {
                title: "t".to_string(),
                rows: Vec::new(),
            })
            .compose(),
            Err(CatalogError::EmptyBlock {
                entry: CatalogEntry::ModalSheet,
            })
        );
        assert_eq!(
            CatalogBlock::Band(StripDescriptor {
                left: vec![Segment::new("")],
                ..StripDescriptor::default()
            })
            .compose(),
            Err(CatalogError::EmptyItem {
                entry: CatalogEntry::Band,
                index: 0,
            })
        );
        assert_eq!(
            CatalogBlock::PillRow(PillRowDescriptor {
                pills: vec![Pill::new("")],
            })
            .compose(),
            Err(CatalogError::EmptyItem {
                entry: CatalogEntry::PillRow,
                index: 0,
            })
        );
    }

    #[test]
    fn pill_delimiters_are_reserved() {
        for label in ["[x]", "a]b", "a[b"] {
            assert_eq!(
                CatalogBlock::PillRow(pills(&[label])).compose(),
                Err(CatalogError::ReservedDelimiter {
                    entry: CatalogEntry::PillRow,
                    index: 0,
                })
            );
        }
    }

    #[test]
    fn mutation_and_ignored_blocks_fail_verification() {
        let block = CatalogBlock::Band(band(&["a", "b"], &["c"], &["d"]));
        let node = block.compose().expect("bounded band");
        assert_eq!(block.verify(&node), Ok(()));

        let UiNode::Row { children, .. } = &node else {
            panic!("band root is a Row");
        };
        let mut mutated = children.clone();
        let UiNode::Row { children: left, .. } = &mut mutated[0] else {
            panic!("left group is a Row");
        };
        left.pop();
        let mutated = UiNode::Row {
            children: mutated,
            fg: None,
            bg: None,
            bold: None,
            on_click: None,
            on_scroll: None,
        };
        assert_eq!(
            block.verify(&mutated),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::Band,
            })
        );

        // A host that ignores the block and presents nothing never passes.
        let ignored = UiNode::row(Vec::new());
        assert_eq!(
            block.verify(&ignored),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::Band,
            })
        );

        let row = CatalogBlock::PillRow(pills(&["a", "b"]));
        let row_node = row.compose().expect("bounded pill row");
        let UiNode::Row { children, .. } = &row_node else {
            panic!("pill row root is a Row");
        };
        let shortened = UiNode::row(children[..1].to_vec());
        assert_eq!(
            row.verify(&shortened),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::PillRow,
            })
        );

        let sheet = CatalogBlock::ModalSheet(ModalSheetDescriptor {
            title: "t".to_string(),
            rows: vec!["a".to_string(), "b".to_string()],
        });
        let sheet_node = sheet.compose().expect("bounded sheet");
        let UiNode::Column { children, .. } = &sheet_node else {
            panic!("sheet root is a Column");
        };
        let shortened = UiNode::column(children[..2].to_vec());
        assert!(sheet.verify(&shortened).is_err());
    }

    #[test]
    fn decorated_trees_are_not_catalog_shapes() {
        let block = CatalogBlock::PillRow(pills(&["a"]));
        let node = block.compose().expect("bounded pill row");
        let UiNode::Row { children, .. } = &node else {
            panic!("pill row root is a Row");
        };
        let UiNode::Text {
            text,
            bg,
            bold,
            on_click,
            on_scroll,
            ..
        } = &children[0]
        else {
            panic!("pill is Text");
        };
        let decorated = UiNode::row(vec![UiNode::Text {
            text: text.clone(),
            fg: Some("accent".to_string()),
            bg: bg.clone(),
            bold: *bold,
            on_click: on_click.clone(),
            on_scroll: on_scroll.clone(),
        }]);
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::PillRow, &decorated),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::PillRow,
            })
        );

        // An explicit `bold = false` is not the canonical absent field.
        let explicit_false = UiNode::row(vec![UiNode::Text {
            text: "[a]".to_string(),
            fg: None,
            bg: None,
            bold: Some(false),
            on_click: None,
            on_scroll: None,
        }]);
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::PillRow, &explicit_false),
            Err(CatalogError::ShapeMismatch {
                entry: CatalogEntry::PillRow,
            })
        );
    }

    #[test]
    fn composed_nodes_carry_no_style_tokens_or_bindings() {
        fn walk(node: &UiNode) {
            match node {
                UiNode::Text {
                    fg,
                    bg,
                    on_click,
                    on_scroll,
                    ..
                } => {
                    assert!(fg.is_none());
                    assert!(bg.is_none());
                    assert!(on_click.is_none());
                    assert!(on_scroll.is_none());
                }
                UiNode::Row {
                    children,
                    fg,
                    bg,
                    bold,
                    on_click,
                    on_scroll,
                }
                | UiNode::Column {
                    children,
                    fg,
                    bg,
                    bold,
                    on_click,
                    on_scroll,
                }
                | UiNode::List {
                    children,
                    fg,
                    bg,
                    bold,
                    on_click,
                    on_scroll,
                } => {
                    assert!(fg.is_none());
                    assert!(bg.is_none());
                    assert!(bold.is_none());
                    assert!(on_click.is_none());
                    assert!(on_scroll.is_none());
                    children.iter().for_each(walk);
                }
            }
        }

        let blocks = [
            CatalogBlock::Band(StripDescriptor {
                left: vec![Segment::emphasized("a")],
                ..StripDescriptor::default()
            }),
            CatalogBlock::PillRow(PillRowDescriptor {
                pills: vec![Pill::active("a")],
            }),
            CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor {
                text: "detail".to_string(),
            }),
            CatalogBlock::ModalSheet(ModalSheetDescriptor {
                title: "title".to_string(),
                rows: vec!["row".to_string()],
            }),
            CatalogBlock::StatusStrip(band(&["a"], &[], &[])),
        ];
        for block in blocks {
            walk(&block.compose().expect("bounded block"));
        }
    }

    #[test]
    fn unused_path_is_byte_identical() {
        // The catalog is not a global hook: a tree that declares no catalog
        // block composes exactly the same bytes before and after the catalog
        // module inspects or refuses it.
        let baseline = UiNode::row(vec![
            UiNode::text("plugin"),
            UiNode::column(vec![UiNode::list(vec![UiNode::text("a")])]),
        ]);
        let before = encode(&baseline);
        let _ = CatalogBlock::from_scene(CatalogEntry::TooltipDetailFallback, &baseline);
        assert_eq!(encode(&baseline), before);
        assert_eq!(before, "R[T(-,N,N,-,-,6:plugin)C[L[T(-,N,N,-,-,1:a)]]]");

        // A declared but empty block is a failure, never a silent no-op.
        assert!(
            CatalogBlock::Band(StripDescriptor::default())
                .compose()
                .is_err()
        );
    }

    #[test]
    fn composition_is_deterministic_and_bounded() {
        let block = CatalogBlock::ModalSheet(ModalSheetDescriptor {
            title: "t".to_string(),
            rows: (0..CATALOG_MAX_ROWS)
                .map(|index| format!("row-{index}"))
                .collect(),
        });
        let first = block.compose().expect("bounded sheet");
        for _ in 0..8 {
            assert_eq!(block.compose().expect("repeat"), first);
        }
        assert_eq!(first.count_nodes(), 1 + (CATALOG_MAX_ROWS + 1) * 2);
        assert_eq!(first.depth(), 3);
        assert!(first.count_nodes() <= CATALOG_MAX_NODES);
        assert!(first.text_bytes() <= CATALOG_MAX_TEXT_BYTES);
    }

    #[test]
    fn recognition_rejects_oversized_and_overdeep_trees() {
        let mut deep = UiNode::text("x");
        for _ in 0..20 {
            deep = UiNode::row(vec![deep]);
        }
        assert_eq!(
            CatalogBlock::from_scene(CatalogEntry::Band, &deep),
            Err(CatalogError::TooDeep {
                found: CATALOG_MAX_DEPTH + 1,
                cap: CATALOG_MAX_DEPTH,
            })
        );

        let wide = UiNode::row((0..=CATALOG_MAX_NODES).map(|_| UiNode::text("x")).collect());
        assert!(matches!(
            CatalogBlock::from_scene(CatalogEntry::Band, &wide),
            Err(CatalogError::TooManyNodes {
                cap: CATALOG_MAX_NODES,
                ..
            })
        ));

        let huge = UiNode::text("x".repeat(CATALOG_MAX_TEXT_BYTES + 1));
        assert!(matches!(
            CatalogBlock::from_scene(CatalogEntry::TooltipDetailFallback, &huge),
            Err(CatalogError::TextTooLong {
                cap: CATALOG_MAX_TEXT_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn composed_kinds_are_the_accepted_v1_set() {
        fn walk(node: &UiNode) {
            assert!(
                UI_V1_NODE_KINDS.contains(&node.kind()),
                "{} is outside the accepted v1 node set",
                node.kind()
            );
            match node {
                UiNode::Text { .. } => {}
                UiNode::Row { children, .. }
                | UiNode::Column { children, .. }
                | UiNode::List { children, .. } => children.iter().for_each(walk),
            }
        }
        let blocks = [
            CatalogBlock::Band(band(&["a"], &["b"], &["c"])),
            CatalogBlock::PillRow(pills(&["a"])),
            CatalogBlock::TooltipDetailFallback(DetailFallbackDescriptor {
                text: "detail".to_string(),
            }),
            CatalogBlock::ModalSheet(ModalSheetDescriptor {
                title: "title".to_string(),
                rows: vec!["row".to_string()],
            }),
            CatalogBlock::StatusStrip(band(&["a"], &[], &[])),
        ];
        for block in blocks {
            walk(&block.compose().expect("bounded block"));
        }
    }
}
