//! Named cross-workspace scratchpad slots (issue #3205, CTX-1862).
//!
//! A scratchpad is a **presentation mode of the same [`View`]**, never a new
//! panel type and never a second layout manager. Every named slot
//! ([`Scratchpads`]) holds at most one [`ScratchpadEntry`]: a live leaf that
//! is detached from every layout tree while the slot owns it.
//!
//! # Show / hide / restore
//!
//! - **Show** stamps the entry [`PresentationMode::Floating`]: the runtime
//!   composites it over the active workspace scene at
//!   [`OverlayTier::Float`](crate::layout::OverlayTier), cross-workspace, so
//!   switching workspaces never hides it.
//! - **Hide** stamps it [`PresentationMode::Scratchpad`] and keeps it parked
//!   in the slot. The entry keeps its [`View`] (and therefore its `ViewId`),
//!   so the runtime's pane/plugin session keyed by that id stays alive: a
//!   scratchpad is not a PTY, and hiding never kills one.
//! - **To tiled** ([`Scratchpads::to_tiled`]) returns the entry to the live
//!   layout beside the anchor recorded when it was parked and stamps it
//!   [`PresentationMode::Tiled`]. This is the explicit "give it back to the
//!   tiling" verb, distinct from show/hide.
//! - **Park** ([`Scratchpads::park`]) moves an existing layout leaf into an
//!   empty named slot and shows it. The vacated slot collapses because the
//!   leaf leaves the tree, exactly like the pinned/detached-float stores.
//!
//! The store itself never paints and never enters the layout solver; it only
//! stores detached leaves. Multi-name bookkeeping is ordered by first park so
//! present paint order and command output stay deterministic.
//!
//! # Distinct from `Visibility`
//!
//! The parked state here is storage (the leaf is out of every tree), never a
//! display flag. `Visibility::ScratchpadHidden` (computed display state in the
//! `bitty-runtime` registry) is deliberately untouched: this module never
//! reads or writes `Visibility`, and no variant is added anywhere.
//!
//! Deterministic, headless, bounded ([`MAX_SCRATCHPADS`]); this module adds no
//! new crate dependency.

#![forbid(unsafe_code)]

use crate::geometry::SplitAxis;
use crate::layout::LayoutNode;
use crate::presentation::PresentationMode;
use crate::view::{View, ViewId};

/// Name of the scratchpad the bare `toggle_scratchpad` verb addresses.
pub const DEFAULT_SCRATCHPAD: &str = "default";

/// Upper bound on named scratchpad slots (bounded against untrusted input).
pub const MAX_SCRATCHPADS: usize = 32;

/// Upper bound on a scratchpad slot name in bytes.
pub const MAX_SCRATCHPAD_NAME_LEN: usize = 32;

/// Workspace command toggling the default scratchpad slot.
///
/// `<owner>.<name>:<command>` grammar (see
/// [`QualifiedCommand`](crate::panel::QualifiedCommand)). Retained for the
/// existing `command:bitty.workspace:scratchpad-toggle` binding; it is the
/// [`DEFAULT_SCRATCHPAD`] slot without a name suffix. Named slots are reached
/// through the keymap action grammar (`toggle_scratchpad:<name>`,
/// `move_to_scratchpad:<name>`, `scratchpad_to_tiled:<name>`), which is the
/// only surface `bitty-terminal` routes.
pub const SCRATCHPAD_CMD_TOGGLE: &str = "bitty.workspace:scratchpad-toggle";

/// Per-scratchpad size/position overrides, resolved against the global
/// `floating` config by the caller (`bitty-runtime`).
///
/// Fractions are the same units as [`FloatStore`](crate::floated::FloatStore)
/// geometry: width/height in `(0.0, 1.0]` of the container, `x`/`y` in
/// `[0.0, 1.0]` when explicitly positioned, and `centered` ignored when both
/// ratios are `None`. [`Default`] is the accepted float default (80% of each
/// axis, centered).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScratchpadGeometry {
    /// Width fraction of the container.
    pub width: f32,
    /// Height fraction of the container.
    pub height: f32,
    /// Whether to center the entry in the container.
    pub centered: bool,
    /// Horizontal anchor ratio when not centered.
    pub x: Option<f32>,
    /// Vertical anchor ratio when not centered.
    pub y: Option<f32>,
    /// Optional per-scratchpad backdrop opacity override in `[0.0, 1.0]`.
    pub background_opacity: Option<f32>,
}

impl Default for ScratchpadGeometry {
    fn default() -> Self {
        Self {
            width: crate::presentation::DEFAULT_FLOAT_SIZE_RATIO,
            height: crate::presentation::DEFAULT_FLOAT_SIZE_RATIO,
            centered: true,
            x: None,
            y: None,
            background_opacity: None,
        }
    }
}

/// True when `name` is an accepted scratchpad slot name: 1..=32 ASCII bytes,
/// lowercase letters, digits, `-`, `_`. Rejecting uppercase keeps the keymap
/// action grammar (which lowercases its argument) unambiguous.
#[must_use]
pub fn is_valid_scratchpad_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SCRATCHPAD_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Error for [`Scratchpads`] transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScratchpadError {
    /// `name` is not an accepted slot name.
    InvalidName(String),
    /// The slot already holds an entry; the tree is untouched.
    Occupied(String),
    /// The slot holds no entry; the tree is untouched.
    Vacant(String),
    /// The bounded slot table is full.
    TooManySlots,
    /// No leaf with this id exists; the tree is untouched.
    LeafNotFound(ViewId),
    /// Only `Tiled`/`Floating`/`Scratchpad` leaves park (`Fullscreen` has its
    /// own verb). The tree is untouched.
    UnsupportedMode {
        /// Mode the leaf had before the refusal.
        current: PresentationMode,
    },
    /// Parking would strand an empty live layout (the tree holds a single
    /// leaf); the tree is untouched.
    StrandedLayout,
    /// The [`PresentationMode::can_transition`] gate rejected the mode
    /// stamp; the tree/layout is restored to its prior shape.
    Rejected {
        /// Mode before the attempted stamp.
        from: PresentationMode,
        /// Mode the transition requested.
        to: PresentationMode,
    },
    /// `command` is not a scratchpad workspace command.
    UnknownCommand(String),
}

impl std::fmt::Display for ScratchpadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(f, "invalid scratchpad name: {name:?}"),
            Self::Occupied(name) => write!(f, "scratchpad '{name}' already holds a panel"),
            Self::Vacant(name) => write!(f, "scratchpad '{name}' is empty"),
            Self::TooManySlots => {
                write!(f, "too many scratchpads: max {MAX_SCRATCHPADS} named slots")
            }
            Self::LeafNotFound(id) => write!(f, "scratchpad target not found: {id}"),
            Self::UnsupportedMode { current } => {
                write!(
                    f,
                    "scratchpad needs a tiled, floating, or scratchpad leaf, found {current}"
                )
            }
            Self::StrandedLayout => f.write_str("scratchpad move would strand an empty layout"),
            Self::Rejected { from, to } => {
                write!(f, "scratchpad transition rejected: {from} -> {to}")
            }
            Self::UnknownCommand(cmd) => write!(f, "unknown scratchpad command: {cmd}"),
        }
    }
}

impl std::error::Error for ScratchpadError {}

/// One named slot: the detached [`View`] plus its restore anchor and
/// per-scratchpad geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct ScratchpadEntry {
    name: String,
    view: View,
    visible: bool,
    anchor: Option<ViewId>,
    after: bool,
    geometry: ScratchpadGeometry,
}

impl ScratchpadEntry {
    /// Slot name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The detached leaf (stamped `Floating` while visible, `Scratchpad`
    /// while hidden).
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The leaf id (stable across hide/show/restore).
    #[must_use]
    pub fn view_id(&self) -> ViewId {
        self.view.id()
    }

    /// Whether the entry is currently shown as a float overlay.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The neighboring leaf id recorded when the entry was parked.
    #[must_use]
    pub fn anchor(&self) -> Option<ViewId> {
        self.anchor
    }

    /// Per-scratchpad geometry overrides.
    #[must_use]
    pub fn geometry(&self) -> ScratchpadGeometry {
        self.geometry
    }
}

/// Window-global named scratchpad store (issue #3205).
///
/// Holds at most one [`ScratchpadEntry`] per name and at most
/// [`MAX_SCRATCHPADS`] names. All transitions are total and deterministic.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scratchpads {
    entries: Vec<ScratchpadEntry>,
}

impl Scratchpads {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// True when no slot holds an entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of occupied slots (visible and hidden).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Occupied slot names in park order.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// True when `name` holds an entry.
    #[must_use]
    pub fn contains_name(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e.name == name)
    }

    /// True when `id` is owned by any slot.
    #[must_use]
    pub fn contains_view(&self, id: ViewId) -> bool {
        self.entries.iter().any(|e| e.view.id() == id)
    }

    /// The entry named `name`, if occupied.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&ScratchpadEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// The detached leaf named `name`, if occupied.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&View> {
        self.entry(name).map(ScratchpadEntry::view)
    }

    /// Mutable access to the detached leaf named `name` (present-time reflow).
    #[must_use]
    pub fn get_mut(&mut self, name: &str) -> Option<&mut View> {
        self.entries
            .iter_mut()
            .find(|e| e.name == name)
            .map(|e| &mut e.view)
    }

    /// Mutable access to a detached leaf by id.
    #[must_use]
    pub fn find_view_mut(&mut self, id: ViewId) -> Option<&mut View> {
        self.entries
            .iter_mut()
            .find(|e| e.view.id() == id)
            .map(|e| &mut e.view)
    }

    /// The detached leaf with `id`, if any (hidden or visible).
    #[must_use]
    pub fn find_view(&self, id: ViewId) -> Option<&View> {
        self.entries
            .iter()
            .find(|e| e.view.id() == id)
            .map(|e| &e.view)
    }

    /// True when `name` is currently shown as a float overlay.
    #[must_use]
    pub fn is_visible(&self, name: &str) -> bool {
        self.entry(name).is_some_and(ScratchpadEntry::visible)
    }

    /// True when `id` belongs to a slot that is currently shown.
    #[must_use]
    pub fn is_view_visible(&self, id: ViewId) -> bool {
        self.entries.iter().any(|e| e.visible && e.view.id() == id)
    }

    /// Visible entries in park order (present paint order), with their
    /// geometry.
    #[must_use]
    pub fn visible_entries(&self) -> Vec<&ScratchpadEntry> {
        self.entries.iter().filter(|e| e.visible).collect()
    }

    /// Ids of every detached leaf (visible and hidden), in park order.
    #[must_use]
    pub fn ids(&self) -> Vec<ViewId> {
        self.entries.iter().map(|e| e.view.id()).collect()
    }

    /// Ids of visible entries in present paint order.
    #[must_use]
    pub fn visible_ids(&self) -> Vec<ViewId> {
        self.entries
            .iter()
            .filter(|e| e.visible)
            .map(|e| e.view.id())
            .collect()
    }

    /// Parks leaf `id` into an empty slot `name` and shows it.
    ///
    /// Fails closed with the tree untouched for an invalid/occupied name, a
    /// full slot table, a single-leaf layout, an unknown id, an unsupported
    /// mode (`Fullscreen` has its own verb), or a rejected stamp.
    ///
    /// `Tiled`, `Floating`, and an in-tree `Scratchpad`-stamped leaf (a spawn
    /// rule may have stamped it) all park.
    pub fn park(
        &mut self,
        name: &str,
        tree: &mut LayoutNode,
        id: ViewId,
        geometry: ScratchpadGeometry,
    ) -> Result<ViewId, ScratchpadError> {
        self.park_leaf(name, tree, id, geometry, true)
    }

    /// [`Self::park`] with an explicit initial visibility (session restore
    /// and plugin adoption reuse the anchor bookkeeping).
    fn park_leaf(
        &mut self,
        name: &str,
        tree: &mut LayoutNode,
        id: ViewId,
        geometry: ScratchpadGeometry,
        visible: bool,
    ) -> Result<ViewId, ScratchpadError> {
        if !is_valid_scratchpad_name(name) {
            return Err(ScratchpadError::InvalidName(name.to_string()));
        }
        if self.contains_name(name) {
            return Err(ScratchpadError::Occupied(name.to_string()));
        }
        if self.entries.len() >= MAX_SCRATCHPADS {
            return Err(ScratchpadError::TooManySlots);
        }
        if tree.leaf_count() <= 1 {
            return Err(ScratchpadError::StrandedLayout);
        }
        let ids = tree.leaf_ids();
        let pos = ids
            .iter()
            .position(|&v| v == id)
            .ok_or(ScratchpadError::LeafNotFound(id))?;
        // Anchor on the following leaf (restore before it); otherwise the
        // preceding leaf (restore after it); otherwise no anchor.
        let (anchor, after) = if let Some(&next) = ids.get(pos + 1) {
            (Some(next), false)
        } else if pos > 0 {
            (Some(ids[pos - 1]), true)
        } else {
            (None, true)
        };
        let Some(mut view) = tree.remove_leaf(id) else {
            return Err(ScratchpadError::LeafNotFound(id));
        };
        let from = view.presentation();
        if !matches!(
            from,
            PresentationMode::Tiled | PresentationMode::Floating | PresentationMode::Scratchpad
        ) {
            Self::restore_view(tree, view, anchor, after);
            return Err(ScratchpadError::UnsupportedMode { current: from });
        }
        let target = if visible {
            PresentationMode::Floating
        } else {
            PresentationMode::Scratchpad
        };
        if !PresentationMode::request_transition(&mut view, target) {
            Self::restore_view(tree, view, anchor, after);
            return Err(ScratchpadError::Rejected { from, to: target });
        }
        self.entries.push(ScratchpadEntry {
            name: name.to_string(),
            view,
            visible,
            anchor,
            after,
            geometry,
        });
        Ok(id)
    }

    /// Creates an empty visible entry around a freshly allocated leaf
    /// (auto-spawn on first trigger). The leaf has never lived in a tree, so
    /// there is no anchor; `to_tiled` docks it beside the first live leaf.
    ///
    /// Fails closed for an invalid/occupied name or a full slot table.
    pub fn create(
        &mut self,
        name: &str,
        view: View,
        geometry: ScratchpadGeometry,
    ) -> Result<ViewId, ScratchpadError> {
        if !is_valid_scratchpad_name(name) {
            return Err(ScratchpadError::InvalidName(name.to_string()));
        }
        if self.contains_name(name) {
            return Err(ScratchpadError::Occupied(name.to_string()));
        }
        if self.entries.len() >= MAX_SCRATCHPADS {
            return Err(ScratchpadError::TooManySlots);
        }
        let id = view.id();
        let mut view = view;
        let from = view.presentation();
        if !PresentationMode::request_transition(&mut view, PresentationMode::Floating) {
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Floating,
            });
        }
        self.entries.push(ScratchpadEntry {
            name: name.to_string(),
            view,
            visible: true,
            anchor: None,
            after: true,
            geometry,
        });
        Ok(id)
    }

    /// Shows the entry named `name` as a float overlay. Idempotent when it is
    /// already visible.
    pub fn show(&mut self, name: &str) -> Result<ViewId, ScratchpadError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.name == name)
            .ok_or_else(|| ScratchpadError::Vacant(name.to_string()))?;
        let from = entry.view.presentation();
        if !PresentationMode::request_transition(&mut entry.view, PresentationMode::Floating) {
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Floating,
            });
        }
        entry.visible = true;
        Ok(entry.view.id())
    }

    /// Hides the entry named `name`: it stays parked with its `ViewId` (and
    /// therefore its pane/plugin session) intact.
    pub fn hide(&mut self, name: &str) -> Result<ViewId, ScratchpadError> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.name == name)
            .ok_or_else(|| ScratchpadError::Vacant(name.to_string()))?;
        let from = entry.view.presentation();
        if !PresentationMode::request_transition(&mut entry.view, PresentationMode::Scratchpad) {
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Scratchpad,
            });
        }
        entry.visible = false;
        Ok(entry.view.id())
    }

    /// Toggles `name`: shows a hidden entry, hides a visible one. Returns the
    /// entry id and its new visibility.
    pub fn toggle_visibility(&mut self, name: &str) -> Result<(ViewId, bool), ScratchpadError> {
        let visible = self
            .entry(name)
            .ok_or_else(|| ScratchpadError::Vacant(name.to_string()))?
            .visible;
        if visible {
            self.hide(name).map(|id| (id, false))
        } else {
            self.show(name).map(|id| (id, true))
        }
    }

    /// Returns the entry named `name` to the tiling as a fresh split beside
    /// its recorded anchor, stamped `Tiled`.
    ///
    /// The slot is emptied. A stale anchor (leaf closed meanwhile) falls back
    /// to docking beside the first live leaf; an empty tree becomes the
    /// restored leaf.
    pub fn to_tiled(
        &mut self,
        name: &str,
        tree: &mut LayoutNode,
    ) -> Result<ViewId, ScratchpadError> {
        let pos = self
            .entries
            .iter()
            .position(|e| e.name == name)
            .ok_or_else(|| ScratchpadError::Vacant(name.to_string()))?;
        let mut entry = self.entries.remove(pos);
        let from = entry.view.presentation();
        if !PresentationMode::request_transition(&mut entry.view, PresentationMode::Tiled) {
            // Put it back exactly as it was: a refused restore must leave the
            // slot byte-identical.
            let mut view = entry.view;
            let _ = PresentationMode::request_transition(&mut view, from);
            entry.view = view;
            self.entries.insert(pos, entry);
            return Err(ScratchpadError::Rejected {
                from,
                to: PresentationMode::Tiled,
            });
        }
        let id = entry.view.id();
        Self::restore_view(tree, entry.view, entry.anchor, entry.after);
        Ok(id)
    }

    /// Discards the entry owning `id` without restoring it to any tree
    /// (dead-panel teardown after its child exits while parked).
    pub fn discard(&mut self, id: ViewId) -> Option<View> {
        let pos = self.entries.iter().position(|e| e.view.id() == id)?;
        Some(self.entries.remove(pos).view)
    }

    /// Discards the entry named `name` without restoring it to any tree
    /// (rollback after a failed auto-spawn).
    pub fn discard_name(&mut self, name: &str) -> Option<View> {
        let pos = self.entries.iter().position(|e| e.name == name)?;
        Some(self.entries.remove(pos).view)
    }

    /// Removes every entry and returns the detached leaves (session restore
    /// installs a whole new world).
    pub fn drain_views(&mut self) -> Vec<View> {
        self.entries
            .drain(..)
            .map(|entry| entry.view)
            .collect::<Vec<_>>()
    }

    /// Puts `view` back into `tree` beside `anchor` (or a fallback dock).
    fn restore_view(tree: &mut LayoutNode, view: View, anchor: Option<ViewId>, after: bool) {
        if tree.leaf_count() == 0 {
            *tree = LayoutNode::leaf(view);
            return;
        }
        if let Some(a) = anchor {
            if tree.leaf_ids().contains(&a)
                && tree.insert_beside(a, &view, SplitAxis::Horizontal, 0.5, after)
            {
                return;
            }
        }
        // Anchor gone (or never recorded): dock after the first live leaf.
        let first = tree
            .leaf_ids()
            .into_iter()
            .next()
            .expect("non-empty tree has a leaf");
        if !tree.insert_beside(first, &view, SplitAxis::Horizontal, 0.5, true) {
            // Unreachable for a non-empty tree, but never lose the view:
            // wrap the whole tree in a split carrying it.
            let old = std::mem::replace(tree, LayoutNode::stack(Vec::new()));
            *tree = LayoutNode::split(SplitAxis::Horizontal, 0.5, old, LayoutNode::leaf(view));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    fn leaf(id: u64) -> LayoutNode {
        LayoutNode::leaf(View::new(ViewId::new(id), 40, 24))
    }

    fn pair() -> LayoutNode {
        LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(1), leaf(2))
    }

    fn triple() -> LayoutNode {
        LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            leaf(1),
            LayoutNode::split(SplitAxis::Horizontal, 0.5, leaf(2), leaf(3)),
        )
    }

    #[test]
    fn park_detaches_and_shows_as_floating_overlay() {
        let mut tree = pair();
        let mut store = Scratchpads::new();
        assert!(store.is_empty());

        store
            .park(
                DEFAULT_SCRATCHPAD,
                &mut tree,
                ViewId::new(1),
                ScratchpadGeometry::default(),
            )
            .expect("park leaf 1");

        assert_eq!(tree.leaf_ids(), vec![ViewId::new(2)], "slot collapses");
        assert_eq!(store.len(), 1);
        assert!(store.contains_name(DEFAULT_SCRATCHPAD));
        assert!(store.contains_view(ViewId::new(1)));
        assert!(store.is_visible(DEFAULT_SCRATCHPAD));
        assert_eq!(store.visible_ids(), vec![ViewId::new(1)]);
        assert_eq!(
            store
                .get(DEFAULT_SCRATCHPAD)
                .expect("parked")
                .presentation(),
            PresentationMode::Floating,
            "a shown scratchpad paints as a cross-workspace float overlay"
        );
    }

    #[test]
    fn hide_keeps_view_identity_and_show_returns_the_same_view() {
        let mut tree = pair();
        let mut store = Scratchpads::new();
        store
            .park(
                "notes",
                &mut tree,
                ViewId::new(1),
                ScratchpadGeometry::default(),
            )
            .expect("park");
        let parked = store.get("notes").expect("parked").clone();

        let hidden = store.hide("notes").expect("hide");
        assert_eq!(hidden, ViewId::new(1));
        assert!(!store.is_visible("notes"));
        assert!(store.visible_ids().is_empty());
        assert_eq!(
            store.get("notes").expect("still parked").presentation(),
            PresentationMode::Scratchpad
        );
        assert_eq!(
            store.get("notes").expect("still parked").id(),
            parked.id(),
            "hiding parks the same view, so its session id survives"
        );
        assert!(
            tree.leaf_ids() == vec![ViewId::new(2)],
            "hide never re-tiles"
        );

        assert_eq!(store.show("notes").expect("show"), ViewId::new(1));
        assert!(store.is_visible("notes"));
        assert_eq!(
            store.get("notes").expect("shown").presentation(),
            PresentationMode::Floating
        );
        assert!(
            !tree.leaf_ids().contains(&ViewId::new(1)),
            "showing a scratchpad never inserts it into the layout"
        );
    }

    #[test]
    fn named_slots_are_independent_and_bounded() {
        let mut tree = triple();
        let mut store = Scratchpads::new();
        store
            .park(
                "alpha",
                &mut tree,
                ViewId::new(1),
                ScratchpadGeometry::default(),
            )
            .expect("park alpha");
        store
            .park(
                "beta",
                &mut tree,
                ViewId::new(2),
                ScratchpadGeometry::default(),
            )
            .expect("park beta");
        assert_eq!(store.names(), vec!["alpha", "beta"]);
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(3)]);

        store.hide("alpha").expect("hide alpha");
        assert!(!store.is_visible("alpha"));
        assert!(store.is_visible("beta"));
        assert_eq!(store.visible_ids(), vec![ViewId::new(2)]);

        // Occupied names are refused without touching the tree or store.
        let before = store.clone();
        assert_eq!(
            store.park(
                "alpha",
                &mut tree,
                ViewId::new(3),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::Occupied("alpha".to_string()))
        );
        assert_eq!(store, before);
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(3)]);

        // Invalid names are refused the same way.
        assert_eq!(
            store.park(
                "Bad Name",
                &mut tree,
                ViewId::new(3),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::InvalidName("Bad Name".to_string()))
        );
        assert!(!is_valid_scratchpad_name(""));
        assert!(!is_valid_scratchpad_name(
            &"x".repeat(MAX_SCRATCHPAD_NAME_LEN + 1)
        ));
        assert!(is_valid_scratchpad_name("build-logs_2"));

        // The bounded table refuses a 33rd name.
        let mut full_tree = LayoutNode::stack((10..=80).map(leaf).collect());
        let mut full = Scratchpads::new();
        let ids = full_tree.leaf_ids();
        for (index, id) in ids.iter().take(MAX_SCRATCHPADS).enumerate() {
            full.park(
                &format!("s{index}"),
                &mut full_tree,
                *id,
                ScratchpadGeometry::default(),
            )
            .expect("park within bound");
        }
        let remaining = full_tree.leaf_ids()[0];
        assert_eq!(
            full.park(
                "overflow",
                &mut full_tree,
                remaining,
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::TooManySlots)
        );
    }

    #[test]
    fn to_tiled_restores_beside_anchor_and_stamps_tiled() {
        let mut tree = triple();
        let mut store = Scratchpads::new();
        store
            .park(
                "notes",
                &mut tree,
                ViewId::new(2),
                ScratchpadGeometry::default(),
            )
            .expect("park leaf 2");
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(3)]);

        let restored = store.to_tiled("notes", &mut tree).expect("to tiled");
        assert_eq!(restored, ViewId::new(2));
        assert!(store.is_empty());
        // Anchor was leaf 3 (following neighbor): restored before it.
        assert_eq!(
            tree.leaf_ids(),
            vec![ViewId::new(1), ViewId::new(2), ViewId::new(3)]
        );
        assert_eq!(
            tree.find_leaf(ViewId::new(2)).expect("leaf").presentation(),
            PresentationMode::Tiled
        );
        let alloc = tree.layout(Rect::new(0, 0, 80, 24));
        assert_eq!(alloc.len(), 3);
        assert!(
            alloc
                .iter()
                .all(|(_, rect)| !rect.is_empty() && rect.right() <= 80 && rect.bottom() <= 24),
            "every restored leaf is allocated inside the container"
        );
        assert_eq!(
            store.to_tiled("notes", &mut tree),
            Err(ScratchpadError::Vacant("notes".to_string()))
        );
    }

    #[test]
    fn refused_park_leaves_tree_and_store_untouched() {
        let mut sole = leaf(1);
        let mut store = Scratchpads::new();
        assert_eq!(
            store.park(
                DEFAULT_SCRATCHPAD,
                &mut sole,
                ViewId::new(1),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::StrandedLayout)
        );
        assert_eq!(sole.leaf_ids(), vec![ViewId::new(1)]);
        assert!(store.is_empty());

        let mut tree = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(View::with_presentation(
                ViewId::new(1),
                40,
                24,
                PresentationMode::Fullscreen,
            )),
            leaf(2),
        );
        assert_eq!(
            store.park(
                DEFAULT_SCRATCHPAD,
                &mut tree,
                ViewId::new(1),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::UnsupportedMode {
                current: PresentationMode::Fullscreen
            })
        );
        // The rejected leaf returns to its exact slot, store untouched.
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert!(store.is_empty());

        let mut tree = pair();
        assert_eq!(
            store.park(
                DEFAULT_SCRATCHPAD,
                &mut tree,
                ViewId::new(404),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::LeafNotFound(ViewId::new(404)))
        );
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert!(store.is_empty());
    }

    #[test]
    fn create_makes_a_visible_spawn_target_and_discard_rolls_back() {
        let mut store = Scratchpads::new();
        let view = View::new(ViewId::new(9), 80, 24);
        let id = store
            .create("btop", view, ScratchpadGeometry::default())
            .expect("create slot");
        assert_eq!(id, ViewId::new(9));
        assert!(store.is_visible("btop"));
        assert_eq!(
            store.get("btop").expect("entry").presentation(),
            PresentationMode::Floating
        );
        // A second create for the same name is refused.
        assert_eq!(
            store.create(
                "btop",
                View::new(ViewId::new(10), 80, 24),
                ScratchpadGeometry::default()
            ),
            Err(ScratchpadError::Occupied("btop".to_string()))
        );
        let removed = store.discard_name("btop").expect("rollback");
        assert_eq!(removed.id(), ViewId::new(9));
        assert!(store.is_empty());

        // A created slot has no anchor; to_tiled docks beside the first leaf.
        let mut tree = pair();
        store
            .create(
                "lazygit",
                View::new(ViewId::new(11), 80, 24),
                ScratchpadGeometry::default(),
            )
            .expect("create");
        assert_eq!(
            store.to_tiled("lazygit", &mut tree).expect("tile"),
            ViewId::new(11)
        );
        assert_eq!(
            tree.leaf_ids(),
            vec![ViewId::new(1), ViewId::new(11), ViewId::new(2)]
        );
    }

    #[test]
    fn toggle_visibility_hides_then_shows_the_same_entry() {
        let mut tree = pair();
        let mut store = Scratchpads::new();
        let id = store
            .park(
                DEFAULT_SCRATCHPAD,
                &mut tree,
                ViewId::new(1),
                ScratchpadGeometry::default(),
            )
            .expect("park leaf 1");
        assert!(store.is_visible(DEFAULT_SCRATCHPAD));
        assert_eq!(
            store.toggle_visibility(DEFAULT_SCRATCHPAD),
            Ok((id, false)),
            "a visible entry is hidden, not dropped"
        );
        assert!(!store.is_visible(DEFAULT_SCRATCHPAD));
        assert!(
            store.contains_name(DEFAULT_SCRATCHPAD),
            "the slot keeps the entry"
        );
        assert_eq!(
            store.toggle_visibility(DEFAULT_SCRATCHPAD),
            Ok((id, true)),
            "the same entry comes back"
        );
        assert!(store.is_visible(DEFAULT_SCRATCHPAD));
        assert_eq!(store.to_tiled(DEFAULT_SCRATCHPAD, &mut tree), Ok(id));
        assert_eq!(tree.leaf_ids(), vec![ViewId::new(1), ViewId::new(2)]);
        assert_eq!(
            tree.find_leaf(ViewId::new(1)).expect("leaf").presentation(),
            PresentationMode::Tiled,
            "to_tiled stamps the restored leaf"
        );
        assert!(store.is_empty(), "to_tiled empties the slot");
    }
}
