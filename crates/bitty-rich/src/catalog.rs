//! Headless extension component catalog (bitty#2114, CTX-1416).
//!
//! Downstream extensions that want a band, a row of pills, a status strip, a
//! tooltip/detail fallback, or a sheet body currently hand-roll [`SceneNode`]
//! trees. This module is the shared, documented set of those compositions: a
//! typed descriptor per component, one bounded and deterministic lowering to
//! the scene model it composes, and nothing else.
//!
//! It lives next to [`crate::scene`] because that is what it composes: the
//! scene model owns admission (`SCN-1..5`), measuring, and painting, and the
//! catalog adds no vocabulary of its own.
//!
//! # Four-node surface
//!
//! Every composition is built from exactly the accepted Plugin API v1 node set
//! — `Text`, `Row`, `Column`, `List`. No `Styled`, no `Block`, no `Rule`, no
//! `Image`, no `Unknown`. So a catalog tree is a v1 tree by construction:
//! [`SceneNode::schema_version`] reports [`crate::scene::RICH_BLOCK_VERSION`],
//! a v1-only consumer can read it unchanged, and the bridge in
//! `bitty-runtime::scene_bridge` would map it without a new arm.
//!
//! | Composition | Node shape |
//! |---|---|
//! | [`BandSpec`] | `Row[ Text(title), Text(slot) × trailing ]` |
//! | [`PillRowSpec`] | `Row[ Text(pill) × pills ]` |
//! | [`StatusStripSpec`] | `Row[ Row[ Text(label), Text(value) ] × segments ]` |
//! | [`DetailSpec`] | `List { items: lines, ordered: false }` |
//! | [`SheetSpec`] | `Column[ Text(title), Text(line) × lines ]` |
//!
//! [`BandSpec`] and [`PillRowSpec`] lower to the same shape family on purpose.
//! The v1 set has exactly one horizontal container, so nesting one of them to
//! make the two *look* different would invent structure no renderer honours.
//! What actually separates them is the contract each enforces: a band carries
//! a required title and at most [`CATALOG_MAX_BAND_SLOTS`] trailing slots at
//! [`CATALOG_MAX_LABEL_BYTES`] each, while a pill row is a bounded run of
//! short chips at the tighter [`CATALOG_MAX_PILL_LABEL_BYTES`].
//!
//! # Determinism
//!
//! Order is carried, never derived. Every input is a `Vec` in declaration
//! order, the lowering walks it once, and no map, set, sort, or dedup exists on
//! the path — so identical descriptors produce byte-identical trees, and
//! permuting the input permutes the output rather than normalising it. The
//! crate's own test suite asserts both directions, and
//! `tests/catalog.rs` guards the source against a hashed collection appearing
//! here later.
//!
//! # Caps and refusal
//!
//! Each composition has a documented cap on its collection and on each label.
//! The caps are enforced by the typed constructor and **refused**, never
//! truncated or clamped: a caller that passes `cap + 1` gets a
//! [`CatalogError::TooManyItems`] or [`CatalogError::LabelTooLong`] naming the
//! composition, the rejected value, and the cap. `cap` itself is accepted.
//!
//! | Composition | Collection cap | Label cap |
//! |---|---|---|
//! | [`BandSpec`] | [`CATALOG_MAX_BAND_SLOTS`] (8) trailing slots | [`CATALOG_MAX_LABEL_BYTES`] (256 B) |
//! | [`PillRowSpec`] | [`CATALOG_MAX_PILLS`] (16) pills | [`CATALOG_MAX_PILL_LABEL_BYTES`] (32 B) |
//! | [`StatusStripSpec`] | [`CATALOG_MAX_STATUS_SEGMENTS`] (12) segments | [`CATALOG_MAX_LABEL_BYTES`] (256 B) |
//! | [`DetailSpec`] | [`CATALOG_MAX_DETAIL_LINES`] (32) lines | [`CATALOG_MAX_LABEL_BYTES`] (256 B) |
//! | [`SheetSpec`] | [`CATALOG_MAX_SHEET_LINES`] (24) body lines | [`CATALOG_MAX_LABEL_BYTES`] (256 B) |
//!
//! A composition also refuses an empty collection (and an empty required
//! title) with [`CatalogError::Empty`]: there is no meaningful empty band or
//! empty pill row, and a caller that produced one has a bug it should see
//! rather than a node that paints nothing.
//!
//! These per-composition caps are much tighter than the bulk scene budget they
//! sit inside. Worst case is a status strip at 12 segments: 37 nodes, depth 3,
//! 6144 text bytes (`12 × (256 + 256)`) — against `SCN-1`'s 2048 nodes,
//! `SCN-2`'s depth 32, and `SCN-3`'s 256 KiB. One catalog component can never
//! spend a block's budget, so composing several is the caller's only lever, and
//! it stays visible at [`crate::scene::Scene::insert`].
//!
//! # Non-goals (deliberate, and pinned by tests)
//!
//! - **No input authority.** No descriptor carries a click, hover, scroll, or
//!   command binding. A composition is inert layout; binding it to a plugin
//!   command is the mount path's decision, not this catalog's.
//! - **No style fields.** Nothing here sets [`crate::scene::SceneNodeStyle`],
//!   a border, a colour token, bold, or italic. Every emitted [`StyledSpan`]
//!   is plain (`bold: false`, `italic: false`). Presentation attaches in the
//!   separate style PR; a catalog node that pre-empted it would have to guess.
//! - **No accessibility role authority.** There is no role, landmark, or
//!   heading claim. Labels are plain text and the accessibility projection
//!   derives whatever it derives from the painted bytes.
//! - **No plugin VM, no renderer handle, no I/O.** Pure data in, scene data
//!   out; `build` is total and infallible.
//!
//! # Growth policy
//!
//! The enums here are not `#[non_exhaustive]`, matching [`SceneNode`] in the
//! `0.0.x` series: a new composition or error is an accepted breaking change
//! for a downstream exhaustive match. Adding one is a contract change, not an
//! additive tweak.

use crate::scene::{ListModel, SceneNode, StyledSpan};

/// Inclusive maximum label length, in bytes, for every composition except the
/// pill row's chips.
///
/// Labels are the only unbounded input a caller supplies, so each one is
/// capped well below `SCN-3`'s 256 KiB block text budget: a single label that
/// could spend a block on its own would let one caller starve every
/// composition sharing the block.
pub const CATALOG_MAX_LABEL_BYTES: usize = 256;

/// Inclusive maximum pill label length, in bytes.
///
/// A pill is a short chip, not a paragraph, so the pill row gets a tighter cap
/// than [`CATALOG_MAX_LABEL_BYTES`]. This is the contract difference that
/// separates a pill row from a band even though both lower to a `Row`.
pub const CATALOG_MAX_PILL_LABEL_BYTES: usize = 32;

/// Inclusive maximum trailing slots on a [`BandSpec`], after its title.
pub const CATALOG_MAX_BAND_SLOTS: usize = 8;

/// Inclusive maximum pills in a [`PillRowSpec`].
pub const CATALOG_MAX_PILLS: usize = 16;

/// Inclusive maximum segments in a [`StatusStripSpec`].
pub const CATALOG_MAX_STATUS_SEGMENTS: usize = 12;

/// Inclusive maximum lines in a [`DetailSpec`].
pub const CATALOG_MAX_DETAIL_LINES: usize = 32;

/// Inclusive maximum body lines in a [`SheetSpec`], excluding its title.
pub const CATALOG_MAX_SHEET_LINES: usize = 24;

/// Which composition a [`CatalogError`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompositionKind {
    /// [`BandSpec`].
    Band,
    /// [`PillRowSpec`].
    PillRow,
    /// [`StatusStripSpec`].
    StatusStrip,
    /// [`DetailSpec`].
    Detail,
    /// [`SheetSpec`].
    Sheet,
}

impl CompositionKind {
    /// Stable lowercase name, for diagnostics and test assertions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Band => "band",
            Self::PillRow => "pill_row",
            Self::StatusStrip => "status_strip",
            Self::Detail => "detail",
            Self::Sheet => "sheet",
        }
    }
}

impl std::fmt::Display for CompositionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed refusal from a catalog constructor.
///
/// Every variant is fail-closed: the constructor returns this instead of
/// truncating the collection or the label, so an over-budget component never
/// reaches the scene as a silently shortened one. Values are reported as they
/// were supplied, not clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogError {
    /// The composition's collection (or its required title) was empty.
    Empty {
        /// The composition that refused.
        composition: CompositionKind,
    },
    /// The composition's collection exceeded its cap.
    ///
    /// `cap` identifies the collection: each composition's collection cap is a
    /// distinct constant, so `(composition, cap)` is unambiguous.
    TooManyItems {
        /// The composition that refused.
        composition: CompositionKind,
        /// Items supplied.
        found: usize,
        /// Inclusive cap for this collection.
        cap: usize,
    },
    /// One label exceeded its byte cap.
    LabelTooLong {
        /// The composition that refused.
        composition: CompositionKind,
        /// Zero-based index of the offending label in declaration order.
        slot: usize,
        /// Bytes supplied.
        found_bytes: usize,
        /// Inclusive byte cap for this label.
        cap_bytes: usize,
    },
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { composition } => {
                write!(f, "{composition}: refuses an empty component")
            }
            Self::TooManyItems {
                composition,
                found,
                cap,
            } => write!(f, "{composition}: {found} items exceeds the cap of {cap}"),
            Self::LabelTooLong {
                composition,
                slot,
                found_bytes,
                cap_bytes,
            } => write!(
                f,
                "{composition}: label {slot} is {found_bytes} bytes, over the cap of {cap_bytes}"
            ),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Lowers one label to a plain [`SceneNode::Text`] leaf.
///
/// The single leaf constructor: no bold, no italic, no style wrapper, so no
/// presentation attribute can enter a catalog tree by accident.
fn text(label: &str) -> SceneNode {
    SceneNode::Text(StyledSpan {
        text: label.to_owned(),
        bold: false,
        italic: false,
    })
}

/// Checks one label against its byte cap.
fn check_label(
    composition: CompositionKind,
    slot: usize,
    label: &str,
    cap_bytes: usize,
) -> Result<(), CatalogError> {
    if label.len() > cap_bytes {
        return Err(CatalogError::LabelTooLong {
            composition,
            slot,
            found_bytes: label.len(),
            cap_bytes,
        });
    }
    Ok(())
}

/// Checks one label list against a collection cap and a label cap.
///
/// `first_slot` is the declaration-order index of `labels[0]`, so a caller
/// that emitted a title first keeps one continuous slot numbering across the
/// whole composition.
fn check_slots(
    composition: CompositionKind,
    labels: &[String],
    cap: usize,
    label_cap_bytes: usize,
    first_slot: usize,
) -> Result<(), CatalogError> {
    if labels.len() > cap {
        return Err(CatalogError::TooManyItems {
            composition,
            found: labels.len(),
            cap,
        });
    }
    for (index, label) in labels.iter().enumerate() {
        check_label(
            composition,
            first_slot.saturating_add(index),
            label,
            label_cap_bytes,
        )?;
    }
    Ok(())
}

/// One title slot plus trailing slots on a single line: `Row` of `Text`.
///
/// The bar's left title and right-hand counters are the reference use: title
/// first, then the trailing slots in declaration order. The catalog adds no
/// separator or filler between slots — spacing is the renderer's job — so a
/// band is exactly its declaration.
///
/// Caps: [`CATALOG_MAX_BAND_SLOTS`] trailing slots, [`CATALOG_MAX_LABEL_BYTES`]
/// per label, and a non-empty title. Any breach is refused at
/// [`BandSpec::new`], so [`BandSpec::build`] cannot shorten a band.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BandSpec {
    title: String,
    trailing: Vec<String>,
}

impl BandSpec {
    /// Builds a band, refusing an out-of-bounds declaration.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Empty`] when `title` is empty;
    /// [`CatalogError::TooManyItems`] when `trailing` exceeds
    /// [`CATALOG_MAX_BAND_SLOTS`]; [`CatalogError::LabelTooLong`] when the
    /// title or any trailing slot exceeds [`CATALOG_MAX_LABEL_BYTES`].
    pub fn new(title: impl Into<String>, trailing: Vec<String>) -> Result<Self, CatalogError> {
        let title = title.into();
        if title.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::Band,
            });
        }
        check_label(CompositionKind::Band, 0, &title, CATALOG_MAX_LABEL_BYTES)?;
        check_slots(
            CompositionKind::Band,
            &trailing,
            CATALOG_MAX_BAND_SLOTS,
            CATALOG_MAX_LABEL_BYTES,
            1,
        )?;
        Ok(Self { title, trailing })
    }

    /// The title slot.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The trailing slots, in declaration order.
    #[must_use]
    pub fn trailing(&self) -> &[String] {
        &self.trailing
    }

    /// Lowers to `Row[ Text(title), Text(slot) × trailing ]`.
    ///
    /// Total: every cap was enforced by [`Self::new`].
    #[must_use]
    pub fn build(&self) -> SceneNode {
        let mut children = Vec::with_capacity(1 + self.trailing.len());
        children.push(text(&self.title));
        children.extend(self.trailing.iter().map(|slot| text(slot)));
        SceneNode::Row(children)
    }
}

/// A horizontal run of short labelled chips: `Row` of `Text`.
///
/// Caps: [`CATALOG_MAX_PILLS`] pills at [`CATALOG_MAX_PILL_LABEL_BYTES`] each,
/// and a non-empty run. The tighter label cap is what distinguishes a pill row
/// from a [`BandSpec`] at the contract level; both lower to a `Row` because the
/// v1 set has one horizontal container.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PillRowSpec {
    pills: Vec<String>,
}

impl PillRowSpec {
    /// Builds a pill row, refusing an out-of-bounds declaration.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Empty`] when `pills` is empty;
    /// [`CatalogError::TooManyItems`] when it exceeds [`CATALOG_MAX_PILLS`];
    /// [`CatalogError::LabelTooLong`] when any pill exceeds
    /// [`CATALOG_MAX_PILL_LABEL_BYTES`].
    pub fn new(pills: Vec<String>) -> Result<Self, CatalogError> {
        if pills.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::PillRow,
            });
        }
        check_slots(
            CompositionKind::PillRow,
            &pills,
            CATALOG_MAX_PILLS,
            CATALOG_MAX_PILL_LABEL_BYTES,
            0,
        )?;
        Ok(Self { pills })
    }

    /// The pills, in declaration order.
    #[must_use]
    pub fn pills(&self) -> &[String] {
        &self.pills
    }

    /// Lowers to `Row[ Text(pill) × pills ]`.
    ///
    /// Total: every cap was enforced by [`Self::new`].
    #[must_use]
    pub fn build(&self) -> SceneNode {
        SceneNode::Row(self.pills.iter().map(|pill| text(pill)).collect())
    }
}

/// One labelled key/value pair in a [`StatusStripSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatusSegment {
    label: String,
    value: String,
}

impl StatusSegment {
    /// Creates a segment; both fields are capped at
    /// [`CATALOG_MAX_LABEL_BYTES`] by [`StatusStripSpec::new`].
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }

    /// The label half.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The value half.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// A horizontal run of labelled key/value segments: `Row` of `Row` of `Text`.
///
/// Each segment is its own two-leaf group so a status reader sees the label
/// and the value as separate text runs instead of one concatenation the
/// catalog would have had to punctuate.
///
/// Caps: [`CATALOG_MAX_STATUS_SEGMENTS`] segments at
/// [`CATALOG_MAX_LABEL_BYTES`] per label and per value, and a non-empty run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StatusStripSpec {
    segments: Vec<StatusSegment>,
}

impl StatusStripSpec {
    /// Builds a status strip, refusing an out-of-bounds declaration.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Empty`] when `segments` is empty;
    /// [`CatalogError::TooManyItems`] when it exceeds
    /// [`CATALOG_MAX_STATUS_SEGMENTS`]; [`CatalogError::LabelTooLong`] when a
    /// label or value exceeds [`CATALOG_MAX_LABEL_BYTES`]. A value's slot index
    /// is its segment index, so `slot` names the segment either way.
    pub fn new(segments: Vec<StatusSegment>) -> Result<Self, CatalogError> {
        if segments.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::StatusStrip,
            });
        }
        if segments.len() > CATALOG_MAX_STATUS_SEGMENTS {
            return Err(CatalogError::TooManyItems {
                composition: CompositionKind::StatusStrip,
                found: segments.len(),
                cap: CATALOG_MAX_STATUS_SEGMENTS,
            });
        }
        for (slot, segment) in segments.iter().enumerate() {
            check_label(
                CompositionKind::StatusStrip,
                slot,
                &segment.label,
                CATALOG_MAX_LABEL_BYTES,
            )?;
            check_label(
                CompositionKind::StatusStrip,
                slot,
                &segment.value,
                CATALOG_MAX_LABEL_BYTES,
            )?;
        }
        Ok(Self { segments })
    }

    /// The segments, in declaration order.
    #[must_use]
    pub fn segments(&self) -> &[StatusSegment] {
        &self.segments
    }

    /// Lowers to `Row[ Row[ Text(label), Text(value) ] × segments ]`.
    ///
    /// Total: every cap was enforced by [`Self::new`].
    #[must_use]
    pub fn build(&self) -> SceneNode {
        SceneNode::Row(
            self.segments
                .iter()
                .map(|segment| SceneNode::Row(vec![text(&segment.label), text(&segment.value)]))
                .collect(),
        )
    }
}

/// A bounded vertical detail block: `List` of `Text` items, unordered.
///
/// This is the tooltip/detail fallback: when a component cannot render the
/// richer thing it wanted, it shows the same facts as a plain list. [`List`]
/// is the v1 node for exactly this — the renderer emits one row per item with
/// no ordinal — so the fallback is a first-class composition rather than a
/// hand-built `Column` a caller has to keep in sync.
///
/// Caps: [`CATALOG_MAX_DETAIL_LINES`] lines at [`CATALOG_MAX_LABEL_BYTES`] each,
/// and a non-empty list.
///
/// [`List`]: SceneNode::List
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DetailSpec {
    lines: Vec<String>,
}

impl DetailSpec {
    /// Builds a detail block, refusing an out-of-bounds declaration.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Empty`] when `lines` is empty;
    /// [`CatalogError::TooManyItems`] when it exceeds
    /// [`CATALOG_MAX_DETAIL_LINES`]; [`CatalogError::LabelTooLong`] when any
    /// line exceeds [`CATALOG_MAX_LABEL_BYTES`].
    pub fn new(lines: Vec<String>) -> Result<Self, CatalogError> {
        if lines.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::Detail,
            });
        }
        check_slots(
            CompositionKind::Detail,
            &lines,
            CATALOG_MAX_DETAIL_LINES,
            CATALOG_MAX_LABEL_BYTES,
            0,
        )?;
        Ok(Self { lines })
    }

    /// The detail lines, in declaration order.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Lowers to `List { items: lines, ordered: false }`.
    ///
    /// Total: every cap was enforced by [`Self::new`].
    #[must_use]
    pub fn build(&self) -> SceneNode {
        SceneNode::List(ListModel {
            items: self.lines.clone(),
            ordered: false,
        })
    }
}

/// A titled vertical body: `Column` of `Text`.
///
/// The shape a modal sheet needs, and no more. It is deliberately named a
/// sheet and not a modal: focus, dismissal, capture, and stacking are host
/// authority this catalog does not have, and inventing a "modal" descriptor
/// that paints a stacked column would claim an authority the node set cannot
/// carry. A host that owns focus mounts the sheet inside its own overlay.
///
/// Caps: [`CATALOG_MAX_SHEET_LINES`] body lines at [`CATALOG_MAX_LABEL_BYTES`]
/// each, a non-empty title, and a non-empty body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SheetSpec {
    title: String,
    lines: Vec<String>,
}

impl SheetSpec {
    /// Builds a sheet, refusing an out-of-bounds declaration.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Empty`] when `title` or `lines` is empty;
    /// [`CatalogError::TooManyItems`] when `lines` exceeds
    /// [`CATALOG_MAX_SHEET_LINES`]; [`CatalogError::LabelTooLong`] when the
    /// title or any body line exceeds [`CATALOG_MAX_LABEL_BYTES`].
    pub fn new(title: impl Into<String>, lines: Vec<String>) -> Result<Self, CatalogError> {
        let title = title.into();
        if title.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::Sheet,
            });
        }
        if lines.is_empty() {
            return Err(CatalogError::Empty {
                composition: CompositionKind::Sheet,
            });
        }
        check_label(CompositionKind::Sheet, 0, &title, CATALOG_MAX_LABEL_BYTES)?;
        check_slots(
            CompositionKind::Sheet,
            &lines,
            CATALOG_MAX_SHEET_LINES,
            CATALOG_MAX_LABEL_BYTES,
            1,
        )?;
        Ok(Self { title, lines })
    }

    /// The title slot.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The body lines, in declaration order.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Lowers to `Column[ Text(title), Text(line) × lines ]`.
    ///
    /// Total: every cap was enforced by [`Self::new`].
    #[must_use]
    pub fn build(&self) -> SceneNode {
        let mut children = Vec::with_capacity(1 + self.lines.len());
        children.push(text(&self.title));
        children.extend(self.lines.iter().map(|line| text(line)));
        SceneNode::Column(children)
    }
}

/// Any validated catalog component, for a caller that keeps a heterogeneous
/// set of them.
///
/// The wrapper only names the set: it forwards [`Self::kind`] and
/// [`Self::build`] and adds no behaviour, so a tree built through it is the
/// same tree the specific descriptor builds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CatalogComponent {
    /// A [`BandSpec`].
    Band(BandSpec),
    /// A [`PillRowSpec`].
    PillRow(PillRowSpec),
    /// A [`StatusStripSpec`].
    StatusStrip(StatusStripSpec),
    /// A [`DetailSpec`].
    Detail(DetailSpec),
    /// A [`SheetSpec`].
    Sheet(SheetSpec),
}

impl CatalogComponent {
    /// Which composition this is.
    #[must_use]
    pub const fn kind(&self) -> CompositionKind {
        match self {
            Self::Band(_) => CompositionKind::Band,
            Self::PillRow(_) => CompositionKind::PillRow,
            Self::StatusStrip(_) => CompositionKind::StatusStrip,
            Self::Detail(_) => CompositionKind::Detail,
            Self::Sheet(_) => CompositionKind::Sheet,
        }
    }

    /// Lowers to the composition's scene subtree.
    ///
    /// Total: the descriptor was validated at construction.
    #[must_use]
    pub fn build(&self) -> SceneNode {
        match self {
            Self::Band(spec) => spec.build(),
            Self::PillRow(spec) => spec.build(),
            Self::StatusStrip(spec) => spec.build(),
            Self::Detail(spec) => spec.build(),
            Self::Sheet(spec) => spec.build(),
        }
    }
}
