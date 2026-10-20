//! Accessibility host over the `bitty-a11y` `SnapshotHost` seam (CTX-1362).
//!
//! The snapshot vocabulary (`Role`, `SceneKind`, `ChromeKind`,
//! `SnapshotBuilder`, `Adapter`, bounds, error mapping) is owned by
//! `bitty-a11y` and is not redefined here. Core implements
//! [`bitty_a11y::SnapshotHost`] over its real terminal projection through
//! [`UiSnapshotHost`] and maps its retained primitives to the extension role
//! vocabulary through [`a11y_role_of`].
//!
//! What this module provides:
//!
//! - [`a11y_role_of`] — the single accessibility mapping for every
//!   [`crate::uitree::UiNodeKind`] primitive, re-exported from [`crate::uitree`]
//!   and returning the extension [`Role`] vocabulary.
//! - [`terminal_runs`] — readable text runs from a terminal snapshot, with
//!   the stated [`FIDELITY_BOUNDARY`]. Styling, graphics protocols, and
//!   images are never promised as structure.
//! - [`UiSnapshotHost`] — owned-values-only host state implementing
//!   [`bitty_a11y::SnapshotHost`]. It retains no `Snapshot`, `HandleMap`,
//!   `ElementHandle`, or `FocusOwner` beyond one ingest call; only persistent
//!   identity (row ordinals, producer scene identities, kind and purpose
//!   spellings, accessible names, the `Generation`) persists across ingests.
//! - [`A11ySettings`] — core-owned reduced-motion plus high-contrast
//!   composition over [`MotionSpec`](crate::motion::MotionSpec) and theme
//!   tokens. The extension crate is dependency-free and cannot own these; they
//!   stay here by construction and never take plugin input.
//!
//! Bounded inputs (fail-closed, never silently truncated, except the
//! announcement queue's documented drop-oldest overflow):
//!
//! - Names, labels, chrome state: at most 256 characters
//!   ([`MAX_ACCESSIBLE_NAME_LEN`](bitty_a11y::MAX_ACCESSIBLE_NAME_LEN)).
//! - Nodes per snapshot: at most 4096
//!   ([`MAX_SNAPSHOT_NODES`](bitty_a11y::MAX_SNAPSHOT_NODES)).
//! - Announcement text: at most 256 characters
//!   ([`MAX_ANNOUNCEMENT_TEXT_LEN`](bitty_a11y::MAX_ANNOUNCEMENT_TEXT_LEN)).
//! - Pending announcements: at most 8
//!   ([`MAX_PENDING_ANNOUNCEMENTS`](bitty_a11y::MAX_PENDING_ANNOUNCEMENTS)).
//!   A full queue drops the oldest notice so the newest state wins (W-134);
//!   overflow does not fail the ingest.
//!
//! The projection is derived and read-only: every constructor takes shared
//! references and returns owned data. It never mutates terminal state, is
//! never persisted, and is never consulted for routing or dispatch. Removing
//! the adapter removes platform exposure only; this host keeps working with
//! zero backends and in safe mode.

#![forbid(unsafe_code)]

use bitty_a11y::{
    A11yError, AnnouncementKind, ChromeKind, CursorPosition, FocusOwner, HandleMap,
    InteractivePurpose, MAX_ACCESSIBLE_NAME_LEN, SceneKind, Snapshot, SnapshotBuilder,
    SnapshotHost, TextRun,
};

use crate::motion::MotionSpec;
use crate::theme::{SAFE_BORDER_FOCUSED, SAFE_BORDER_IDLE, TokenColor};

/// Re-export of the extension's fixed v1 role vocabulary.
///
/// Core code imports `Role` from this module, so `bitty_a11y` is named in
/// exactly one place (`a11y_host.rs`) and the W-147 source fence stays
/// meaningful outside the accessibility seam.
pub use bitty_a11y::Role;

// ---------------------------------------------------------------------------
// Fidelity boundary (stated, not implied)
// ---------------------------------------------------------------------------

/// Stated fidelity boundary for terminal exposure.
///
/// The projection exposes readable text and the cursor position; it does
/// not promise faithful representation of absolute cursor addressing,
/// full SGR styling as a semantic structure, graphics protocols, or
/// images. The boundary is stated rather than implied, so no reviewer
/// mistakes partial exposure for a missing feature.
pub const FIDELITY_BOUNDARY: &str = "terminal exposure covers readable text runs and cursor position only; \
absolute cursor addressing, SGR styling as semantic structure, graphics protocols, and images are not represented";

/// Core-owned accessible name used when an untrusted PTY title is overlong.
///
/// The OSC title is bounded by the parser at 4096 bytes but the accessibility
/// name ceiling is 256 characters. Passing an overlong title to the extension
/// would fail every ingest until the PTY changes it, leaving assistive
/// technology with a stale or absent tree. Core therefore substitutes this
/// fixed name for both the window root and the terminal region; the overlong
/// title itself is never truncated or exposed.
pub const FALLBACK_ACCESSIBLE_NAME: &str = "terminal";

// ---------------------------------------------------------------------------
// Accessibility mapping for retained primitives
// ---------------------------------------------------------------------------

/// Re-export of the single primitive-to-role table owned by [`crate::uitree`].
///
/// Keeping the table next to [`crate::uitree::UiNodeKind`] avoids a second
/// copy in the a11y host; the host only names the extension [`Role`]
/// vocabulary.
pub use crate::uitree::a11y_role_of;

// ---------------------------------------------------------------------------
// Terminal projection (text runs only, never styling)
// ---------------------------------------------------------------------------

/// Reads the text runs of a terminal snapshot: one [`TextRun`] per non-blank
/// row, in row order, with trailing blanks trimmed.
///
/// Read-only: the snapshot is only borrowed. Wide-character trailing halves
/// (spacers) are skipped, never split; combining marks stay attached to
/// their base cell. Styling is deliberately absent (see
/// [`FIDELITY_BOUNDARY`]). The returned rows are the same grid truth the
/// renderer consumes: the projection reads the live `Snapshot`, never a
/// copy or a second terminal state.
#[must_use]
pub fn terminal_runs(snapshot: &bitty_term_state::Snapshot) -> Vec<TextRun> {
    let mut runs = Vec::new();
    if snapshot.width == 0 || snapshot.height == 0 {
        return runs;
    }
    for row in 0..snapshot.height {
        let start = row.saturating_mul(snapshot.width);
        let end = start
            .saturating_add(snapshot.width)
            .min(snapshot.cells.len());
        let Some(cells) = snapshot.cells.get(start..end) else {
            continue;
        };
        let mut text = String::new();
        for cell in cells {
            if cell.spacer {
                continue;
            }
            // CTX-1367 (#2266): emit the glyph plus every zero-width scalar
            // (combining marks, VS16, ZWJ) through the cell helper so the
            // accessibility run matches the copy path byte-for-byte.
            cell.push_text_into(&mut text);
        }
        let trimmed = text.trim_end().to_string();
        if !trimmed.is_empty() {
            runs.push(TextRun {
                row: u32::try_from(row).unwrap_or(u32::MAX),
                text: trimmed,
            });
        }
    }
    runs
}

// ---------------------------------------------------------------------------
// Snapshot host over real UI state
// ---------------------------------------------------------------------------

/// Owned-values-only accessibility host over Core UI state.
///
/// Holds row ordinals, producer-assigned scene identities, kind and purpose
/// spellings, accessible names, cursor position, and notices — never a
/// `Snapshot`, `HandleMap`, `ElementHandle`, or `FocusOwner`. Each ingest
/// re-binds focus fresh through [`SnapshotHost::focus_for`]; the adapter owns
/// the live projection and retains nothing on the host side.
///
/// Focus is Core-owned: [`UiSnapshotHost::set_focus_label`] records the
/// accessible name of the surface Core reports as focused, or [`None`] when
/// nothing is focused. The host never infers focus from a render, a cursor,
/// or a default terminal attachment.
#[derive(Clone, Debug)]
pub struct UiSnapshotHost {
    root_name: String,
    title: String,
    rows: Vec<(u32, String)>,
    cursor_row: u32,
    cursor_col: u32,
    cursor_visible: bool,
    chrome: Vec<(ChromeKind, String, Option<String>)>,
    scene: Vec<(SceneKind, u64)>,
    interactive: Vec<(InteractivePurpose, String, bool)>,
    modal_overlay: bool,
    focus_label: Option<String>,
    notices: Vec<(AnnouncementKind, String)>,
}

impl UiSnapshotHost {
    /// Starts a host naming the window root and the terminal region.
    pub fn new(root_name: &str, title: &str) -> Self {
        Self {
            root_name: root_name.to_string(),
            title: title.to_string(),
            rows: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            chrome: Vec::new(),
            scene: Vec::new(),
            interactive: Vec::new(),
            modal_overlay: false,
            focus_label: None,
            notices: Vec::new(),
        }
    }

    /// Projects the terminal leaf (title, text runs, cursor) from a live
    /// terminal snapshot.
    ///
    /// Chrome, scene, interactive, modal, focus, and notices stay empty
    /// until the caller pushes them: this constructor never invents focus or
    /// an interactive surface that Core did not declare. The snapshot is
    /// only borrowed and is not retained.
    #[must_use]
    pub fn from_terminal(snapshot: &bitty_term_state::Snapshot) -> Self {
        let title = snapshot.title.as_str();
        let title = if title.chars().count() > MAX_ACCESSIBLE_NAME_LEN {
            FALLBACK_ACCESSIBLE_NAME
        } else {
            title
        };
        let mut host = Self::new(title, title);
        host.rows = terminal_runs(snapshot)
            .into_iter()
            .map(|run| (run.row, run.text))
            .collect();
        host.cursor_row = u32::from(snapshot.cursor.position.row);
        host.cursor_col = u32::from(snapshot.cursor.position.col);
        host.cursor_visible = snapshot.cursor.visible;
        host
    }

    /// Sets the live cursor position and visibility.
    #[must_use]
    pub const fn with_cursor(mut self, row: u32, col: u32, visible: bool) -> Self {
        self.cursor_row = row;
        self.cursor_col = col;
        self.cursor_visible = visible;
        self
    }

    /// Adds a terminal row ordinal plus its readable text.
    pub fn push_row(&mut self, row: u32, text: &str) {
        self.rows.push((row, text.to_string()));
    }

    /// Adds a chrome surface with its accessible name and optional state.
    pub fn push_chrome(&mut self, kind: ChromeKind, name: &str, state: Option<&str>) {
        self.chrome
            .push((kind, name.to_string(), state.map(str::to_string)));
    }

    /// Adds a scene node by kind with its producer-assigned identity.
    pub fn push_scene(&mut self, kind: SceneKind, producer_identity: u64) {
        self.scene.push((kind, producer_identity));
    }

    /// Adds an interactive node with its accessible name and enabled state.
    pub fn push_interactive(&mut self, purpose: InteractivePurpose, name: &str, enabled: bool) {
        self.interactive.push((purpose, name.to_string(), enabled));
    }

    /// Declares a modal overlay: while set, focus is confined to the overlay
    /// surface and the build fails closed without one.
    pub const fn set_modal_overlay(&mut self, active: bool) {
        self.modal_overlay = active;
    }

    /// Records the accessible name of the surface Core reports as focused.
    ///
    /// `None` means nothing is focused (unfocused window, empty workspace, or
    /// a just-detached surface). The name is resolved against the finished
    /// snapshot to the first focusable match in tree order; root, chrome, and
    /// scene nodes can never own focus. An unknown label fails closed with
    /// [`A11yError::UnknownHandle`], never a guessed owner.
    pub fn set_focus_label(&mut self, label: Option<&str>) {
        self.focus_label = label.map(str::to_string);
    }

    /// Queues one notice reported by the next ingest.
    ///
    /// The notice is retained on the host until the host is rebuilt or the
    /// caller clears it; there is deliberately no `clear`/`drain` on this
    /// candidate API yet. A host value reused across ingests therefore
    /// re-reports its accumulated notices, not only the current transition.
    /// The adapter still bounds delivery at
    /// [`MAX_PENDING_ANNOUNCEMENTS`](bitty_a11y::MAX_PENDING_ANNOUNCEMENTS)
    /// with drop-oldest, so this is a reuse limitation, not an unbounded
    /// path (follow-up: bitty#2352).
    pub fn push_notice(&mut self, kind: AnnouncementKind, text: &str) {
        self.notices.push((kind, text.to_string()));
    }
}

impl SnapshotHost for UiSnapshotHost {
    fn build_snapshot(&self, builder: &mut SnapshotBuilder) -> Result<(), A11yError> {
        builder.set_root_name(&self.root_name)?;
        let runs: Vec<TextRun> = self
            .rows
            .iter()
            .map(|(row, text)| TextRun {
                row: *row,
                text: text.clone(),
            })
            .collect();
        builder.set_terminal(
            &self.title,
            runs,
            CursorPosition {
                row: self.cursor_row,
                col: self.cursor_col,
            },
            self.cursor_visible,
        )?;
        for (kind, name, state) in &self.chrome {
            builder.add_chrome(*kind, name, state.as_deref())?;
        }
        for (kind, identity) in &self.scene {
            builder.add_scene(*kind, *identity);
        }
        for (purpose, name, enabled) in &self.interactive {
            builder.add_interactive(*purpose, name, *enabled)?;
        }
        builder.set_modal_overlay(self.modal_overlay);
        Ok(())
    }

    fn focus_for(
        &self,
        snapshot: &Snapshot,
        map: &HandleMap,
    ) -> Result<Option<FocusOwner>, A11yError> {
        let Some(label) = self.focus_label.as_deref() else {
            return Ok(None);
        };
        // Labels are not unique: the window root and the terminal region may
        // share one name. Prefer the first focusable match in tree order
        // (terminal leaf or interactive node); root, chrome, and scene nodes
        // can never own focus.
        let mut matched = false;
        for handle in snapshot.preorder() {
            let Some(view) = snapshot.get(handle) else {
                continue;
            };
            let name = match view {
                bitty_a11y::NodeView::Root { name }
                | bitty_a11y::NodeView::Chrome { name, .. }
                | bitty_a11y::NodeView::Interactive { name, .. } => Some(name),
                bitty_a11y::NodeView::Terminal { title, .. } => Some(title),
                bitty_a11y::NodeView::TerminalRow { .. } | bitty_a11y::NodeView::Scene { .. } => {
                    None
                }
            };
            if name != Some(label) {
                continue;
            }
            matched = true;
            match FocusOwner::bind(snapshot, map, handle) {
                Ok(owner) => return Ok(Some(owner)),
                Err(A11yError::FocusMismatch) => continue,
                Err(other) => return Err(other),
            }
        }
        if matched {
            return Err(A11yError::FocusMismatch);
        }
        Err(A11yError::UnknownHandle)
    }

    fn announcements(&self) -> Vec<(AnnouncementKind, String)> {
        self.notices.clone()
    }
}

// ---------------------------------------------------------------------------
// Settings composition: core-owned, never plugin-overridable
// ---------------------------------------------------------------------------
//
// Both flags are constructed from core configuration only
// (`appearance.animations.reduced_motion`, the high-contrast theme switch):
// no constructor takes a plugin handle, plugin layer, or plugin token, so
// a plugin cannot override either flag by construction.

/// Accessibility settings composing core motion and theme choices.
///
/// `reduced_motion` collapses every animation to its final committed
/// state (see [`A11ySettings::resolve_motion`]); `high_contrast` selects
/// the compliant outline pair (see
/// [`A11ySettings::high_contrast_outlines`]). Both default off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct A11ySettings {
    reduced_motion: bool,
    high_contrast: bool,
}

impl A11ySettings {
    /// Starts with both flags off (animations resolve as authored, theme
    /// tokens resolve through the normal cascade).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reduced_motion: false,
            high_contrast: false,
        }
    }

    /// Sets the core `reduced_motion` flag (builder).
    #[must_use]
    pub const fn with_reduced_motion(mut self, reduced: bool) -> Self {
        self.reduced_motion = reduced;
        self
    }

    /// Sets the core high-contrast flag (builder).
    #[must_use]
    pub const fn with_high_contrast(mut self, high_contrast: bool) -> Self {
        self.high_contrast = high_contrast;
        self
    }

    /// Whether animation must collapse to its final committed state.
    #[must_use]
    pub const fn reduced_motion(self) -> bool {
        self.reduced_motion
    }

    /// Whether the compliant outline pair is selected.
    #[must_use]
    pub const fn high_contrast(self) -> bool {
        self.high_contrast
    }

    /// Resolves `spec` under the reduced-motion flag: when set, the
    /// animation collapses to [`MotionSpec::INSTANT`] (the final committed
    /// state, no interpolation); otherwise `spec` passes through
    /// untouched. Mirrors the [`MotionConfig`](crate::motion::MotionConfig)
    /// short-circuit without taking any plugin input.
    #[must_use]
    pub const fn resolve_motion(self, spec: MotionSpec) -> MotionSpec {
        if self.reduced_motion {
            return MotionSpec::INSTANT;
        }
        spec
    }

    /// The compliant outline pair while high contrast is set: the accepted
    /// `--safe` forced pair (focused white, idle gray), which satisfies
    /// the enforced contrast floors. Returns `None` when high contrast is
    /// off so the caller keeps the normal theme cascade instead.
    #[must_use]
    pub const fn high_contrast_outlines(self) -> Option<(TokenColor, TokenColor)> {
        if self.high_contrast {
            return Some((SAFE_BORDER_FOCUSED, SAFE_BORDER_IDLE));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use bitty_a11y::Generation;

    #[test]
    fn host_holds_owned_values_only() {
        let mut host = UiSnapshotHost::new("window", "shell");
        host.push_row(0, "hello");
        host.push_chrome(ChromeKind::Bar, "status bar", Some("main.rs"));
        host.push_scene(SceneKind::Text, 7);
        host.push_interactive(InteractivePurpose::Button, "Send", true);
        host.set_focus_label(Some("shell"));
        host.push_notice(AnnouncementKind::Focus, "shell focused");
        let generation = Generation::initial();
        let mut adapter = bitty_a11y::Adapter::new();
        adapter
            .ingest_host(&host, generation, None)
            .expect("host ingest");
        assert!(adapter.snapshot().is_some());
    }
}
