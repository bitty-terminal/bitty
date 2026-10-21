//! Configuration dashboard overlay (issue #2380, CTX-1429).
//!
//! A full-panel, Core-owned configuration surface opened from a chrome
//! action (the `toggle_dashboard` action, bound by the user in `init.lua`).
//! It answers the "shortest path to a working setup" question without a
//! shell: the current profile/theme/font/decoration, the installed and
//! enabled plugins, every installed plugin's settings schema/default/current
//! value,
//! the available profiles, and the installed extensions.
//!
//! # Why this lives in Core
//!
//! Bundled plugins are manifest-only today: `PluginRuntimeConfig` ships an
//! empty `bundled_roots` (`bitty-terminal/src/plugin_runtime.rs` documents
//! the first-party packaging follow-up), so there is no way to ship Lua
//! code inside the binary. A "bundled plugin" with Core code behind a
//! manifest id would be exactly the private channel the bundled catalog
//! forbids. The dashboard is therefore a Core presentation surface with no
//! capability of its own, and every write it offers routes through a
//! mutation path that already exists and is already user-initiated:
//!
//! - theme: a live, non-persisting palette preview ([`Runtime::preview_theme_preset`]),
//!   the same `set_theme_palette` path config reload uses;
//! - plugin enable/disable: the managed-manifest operation the
//!   `bitty plugin enable|disable` CLI owns (atomic write + hardening),
//!   dispatched by the app layer, never from here.
//!
//! The panel itself performs no I/O, reads no file, starts no process, and
//! never constructs a plugin runtime or services (settings are read through
//! the bounded plain-config loader). It renders a snapshot the app built on a
//! cold path (open, `r`, or after a write) and returns typed intents.
//!
//! # Bounds and fail-closed rules
//!
//! | Bound | Value | Effect |
//! |-------|-------|--------|
//! | rows per section | [`DASHBOARD_MAX_ROWS`] | tail replaced by a `+N more` note row |
//! | label bytes | [`DASHBOARD_MAX_LABEL_BYTES`] | truncated on a char boundary |
//! | value bytes | [`DASHBOARD_MAX_VALUE_BYTES`] | truncated on a char boundary |
//! | note bytes | [`DASHBOARD_MAX_NOTE_BYTES`] | truncated on a char boundary |
//! | panel | 1-cell margin, min 30x9 | not painted at all when the view is smaller |
//!
//! A section whose data source failed is [`DashboardSectionData::Unavailable`]:
//! it paints the reason and exposes no actionable row, so a missing file or
//! an unreadable registry can never look like "nothing configured". Every
//! mutation requires a second `Enter` on the same row ([`DashboardOutcome::Armed`]
//! then [`DashboardOutcome::Activate`]); any move or section change disarms.
//!
//! # Presentation-only
//!
//! Like the help popup, [`Runtime::paint_dashboard_panel`] pushes fills and
//! glyphs onto the presented frame. It never touches grid cells, scrollback,
//! layout, focus, or terminal state; closing it repaints the next frame.

use super::*;

/// Panel title painted on the first inner row.
pub const DASHBOARD_TITLE: &str = "Configuration dashboard";

/// Idle footer line (ASCII-only so it paints under every font stack).
pub const DASHBOARD_FOOTER_HINT: &str = "j/k move  h/l section  Enter apply  r reload  q/Esc close";

/// Footer line while a mutation awaits its confirming `Enter`.
pub const DASHBOARD_CONFIRM_HINT: &str = "Enter again to confirm  any move cancels";

/// Hint painted when the focused view cannot hold a legible panel.
///
/// Keys stay captured (the panel is open), so the refusal must be visible.
pub const DASHBOARD_TOO_SMALL_HINT: &str = "terminal too small for the configuration dashboard";

/// Label painted for the bounded-overflow tail row.
pub const DASHBOARD_OVERFLOW_LABEL: &str = "+more";

/// Maximum rows retained per section.
///
/// Generous for a curated configuration surface (installed plugins,
/// profiles, extensions) and far below any rendering budget; the excess is
/// reported, never silently dropped.
pub const DASHBOARD_MAX_ROWS: usize = 128;

/// Maximum label bytes per row.
pub const DASHBOARD_MAX_LABEL_BYTES: usize = 48;

/// Maximum value bytes per row.
pub const DASHBOARD_MAX_VALUE_BYTES: usize = 96;

/// Maximum bytes for a value that is a filesystem path.
///
/// Paths are the payload of the Diagnostics section and the Extensions
/// target row, and host temp roots are long (`/var/folders/...` on macOS).
/// The ordinary 96-byte value bound would cut the marker or the executable
/// name; the wider bound still keeps the row bounded, and the paint path
/// clips it to the panel width.
pub const DASHBOARD_MAX_PATH_BYTES: usize = 240;

/// Maximum note bytes per row/section.
pub const DASHBOARD_MAX_NOTE_BYTES: usize = 96;

/// Minimum view width in cells that can hold a legible panel.
const DASHBOARD_MIN_VIEW_W: u16 = 30;

/// Minimum view height in cells that can hold a legible panel.
const DASHBOARD_MIN_VIEW_H: u16 = 9;

/// Closed section set, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DashboardSection {
    /// Profile, theme, font, decoration, and the config file path.
    Overview,
    /// Installed/bundled plugins with their enabled state.
    Plugins,
    /// Settings schema/default/current/source for one selected plugin.
    Settings,
    /// Named profiles and the active one.
    Profiles,
    /// Installed native extensions with their state.
    Extensions,
    /// Platform-resolved paths and versions (read-only diagnostics).
    Diagnostics,
}

impl DashboardSection {
    /// Every section, in canonical tab order.
    pub const ALL: [Self; 6] = [
        Self::Overview,
        Self::Plugins,
        Self::Settings,
        Self::Profiles,
        Self::Extensions,
        Self::Diagnostics,
    ];

    /// Canonical tab title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Plugins => "Plugins",
            Self::Settings => "Settings",
            Self::Profiles => "Profiles",
            Self::Extensions => "Extensions",
            Self::Diagnostics => "Diagnostics",
        }
    }

    /// Index in [`Self::ALL`] (also the tab number `1..=5`).
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Plugins => 1,
            Self::Settings => 2,
            Self::Profiles => 3,
            Self::Extensions => 4,
            Self::Diagnostics => 5,
        }
    }

    /// Section at `index`, clamped into the closed set.
    #[must_use]
    pub fn at(index: usize) -> Self {
        Self::ALL[index.min(Self::ALL.len() - 1)]
    }
}

impl std::fmt::Display for DashboardSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}

/// One typed intent a row can carry.
///
/// The runtime never performs the write itself: [`Runtime::dashboard_key`]
/// returns the intent after the in-panel confirmation, and the app layer
/// applies it through the Core path that owns it (and may still refuse it).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DashboardAction {
    /// Read-only row (informational value or a follow-up pointer).
    #[default]
    None,
    /// Live-apply a built-in theme preset without persisting it.
    PreviewTheme {
        /// Preset name as resolved by `bitty-config` (`bitty-dark`, ...).
        name: String,
    },
    /// Enable or disable an already-recorded plugin.
    ///
    /// `enabled` is the *desired* state; the app re-checks the record and
    /// the bundled pin before writing, so a stale row is refused rather
    /// than applied to a different plugin generation.
    SetPluginEnabled {
        /// Plugin id (fully qualified, e.g. `bitty-terminal.palette`).
        id: String,
        /// Desired enabled state.
        enabled: bool,
    },
}

/// One painted row: left label, right value, optional typed intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardRow {
    /// Left column (bounded by [`DASHBOARD_MAX_LABEL_BYTES`]).
    pub label: String,
    /// Right column (bounded by [`DASHBOARD_MAX_VALUE_BYTES`]).
    pub value: String,
    /// Typed intent executed on the confirming `Enter`.
    pub action: DashboardAction,
}

impl DashboardRow {
    /// Read-only row.
    #[must_use]
    pub fn info(label: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        Self {
            label: bounded_text(label.as_ref(), DASHBOARD_MAX_LABEL_BYTES),
            value: bounded_text(value.as_ref(), DASHBOARD_MAX_VALUE_BYTES),
            action: DashboardAction::None,
        }
    }

    /// Read-only path row (bounded by [`DASHBOARD_MAX_PATH_BYTES`]).
    #[must_use]
    pub fn path(label: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        Self {
            label: bounded_text(label.as_ref(), DASHBOARD_MAX_LABEL_BYTES),
            value: bounded_text(value.as_ref(), DASHBOARD_MAX_PATH_BYTES),
            action: DashboardAction::None,
        }
    }

    /// Actionable row.
    #[must_use]
    pub fn action(label: impl AsRef<str>, value: impl AsRef<str>, action: DashboardAction) -> Self {
        Self {
            label: bounded_text(label.as_ref(), DASHBOARD_MAX_LABEL_BYTES),
            value: bounded_text(value.as_ref(), DASHBOARD_MAX_VALUE_BYTES),
            action,
        }
    }

    /// Whether activating this row can mutate anything.
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        !matches!(self.action, DashboardAction::None)
    }
}

/// Per-section data: rows, or a typed reason why they are unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardSectionData {
    /// Rows were read successfully (possibly empty).
    Ready(Vec<DashboardRow>),
    /// The source failed; nothing in this section is actionable.
    Unavailable {
        /// One-line reason, bounded by [`DASHBOARD_MAX_NOTE_BYTES`].
        reason: String,
    },
}

impl Default for DashboardSectionData {
    fn default() -> Self {
        Self::Unavailable {
            reason: "not loaded".to_string(),
        }
    }
}

impl DashboardSectionData {
    /// Build a ready section, enforcing [`DASHBOARD_MAX_ROWS`].
    ///
    /// Rows past the cap are replaced by one note row
    /// (`+more | N more rows (bounded)`), so truncation is visible.
    #[must_use]
    pub fn ready(mut rows: Vec<DashboardRow>) -> Self {
        if rows.len() > DASHBOARD_MAX_ROWS {
            let hidden = rows.len() - (DASHBOARD_MAX_ROWS - 1);
            rows.truncate(DASHBOARD_MAX_ROWS - 1);
            rows.push(DashboardRow::info(
                DASHBOARD_OVERFLOW_LABEL,
                format!("{hidden} more rows (bounded)"),
            ));
        }
        Self::Ready(rows)
    }

    /// Build an unavailable section with a bounded reason.
    #[must_use]
    pub fn unavailable(reason: impl AsRef<str>) -> Self {
        Self::Unavailable {
            reason: bounded_text(reason.as_ref(), DASHBOARD_MAX_NOTE_BYTES),
        }
    }

    /// Whether the section carries a readable answer.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// Rows for a ready section; empty for an unavailable one.
    #[must_use]
    pub fn rows(&self) -> &[DashboardRow] {
        match self {
            Self::Ready(rows) => rows,
            Self::Unavailable { .. } => &[],
        }
    }

    /// Navigation length: rows for ready sections, one synthetic
    /// "unavailable" row for failed ones (so it is visible and selectable
    /// but never actionable).
    #[must_use]
    pub fn nav_len(&self) -> usize {
        match self {
            Self::Ready(rows) => rows.len(),
            Self::Unavailable { .. } => 1,
        }
    }
}

/// Immutable snapshot the app builds on a cold path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardSnapshot {
    sections: [DashboardSectionData; DashboardSection::ALL.len()],
}

impl Default for DashboardSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl DashboardSnapshot {
    /// Snapshot with every section unavailable (`not loaded`).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            sections: std::array::from_fn(|_| DashboardSectionData::default()),
        }
    }

    /// Snapshot from five section payloads, in tab order.
    #[must_use]
    pub fn new(
        overview: DashboardSectionData,
        plugins: DashboardSectionData,
        settings: DashboardSectionData,
        profiles: DashboardSectionData,
        extensions: DashboardSectionData,
        diagnostics: DashboardSectionData,
    ) -> Self {
        Self {
            sections: [
                overview,
                plugins,
                settings,
                profiles,
                extensions,
                diagnostics,
            ],
        }
    }

    /// Section payload.
    #[must_use]
    pub fn section(&self, section: DashboardSection) -> &DashboardSectionData {
        &self.sections[section.index()]
    }

    /// Replace one section payload (the app uses this to swap the settings
    /// section when the selected plugin changes).
    pub fn set_section(&mut self, section: DashboardSection, data: DashboardSectionData) {
        self.sections[section.index()] = data;
    }

    /// Whether any section carries readable data.
    #[must_use]
    pub fn has_data(&self) -> bool {
        self.sections.iter().any(DashboardSectionData::is_ready)
    }
}

/// One keypress mapped by the app into the closed dashboard vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardKey {
    /// Move selection up (`k`, `Up`).
    Up,
    /// Move selection down (`j`, `Down`).
    Down,
    /// Move one page up (`PageUp`).
    PageUp,
    /// Move one page down (`PageDown`).
    PageDown,
    /// First row (`Home`, `g`).
    First,
    /// Last row (`End`, `G`).
    Last,
    /// Next section (`l`, `Right`, `Tab`).
    NextSection,
    /// Previous section (`h`, `Left`, `BackTab`).
    PrevSection,
    /// Jump to section `n` (`1..=5`).
    Jump(u8),
    /// Activate the selected row (`Enter`), arming a write first.
    Activate,
    /// Request a fresh snapshot (`r`).
    Reload,
    /// Close the panel (`q`, `Esc`).
    Close,
}

/// Result of one [`Runtime::dashboard_key`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardOutcome {
    /// The press does nothing (no rows, unknown jump index, ...).
    Ignored,
    /// State changed; repaint on the next frame.
    Redraw,
    /// A write is armed; a second `Activate` on the same row performs it.
    Armed,
    /// The selected row's intent is confirmed and ready to apply.
    Activate(DashboardAction),
    /// The panel asks the app for a fresh snapshot.
    Reload,
    /// The panel closed.
    Close,
}

/// Armed-confirmation identity: section + row position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmedConfirm {
    section: usize,
    row: usize,
}

/// Dashboard visible/section/cursor/confirmation state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DashboardState {
    visible: bool,
    section: usize,
    cursor: usize,
    armed: Option<ArmedConfirm>,
    /// Body height reported by the last paint (page-step size).
    last_page: usize,
    snapshot: DashboardSnapshot,
}

impl DashboardState {
    /// Whether the panel is open.
    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Active section.
    #[must_use]
    pub fn section(&self) -> DashboardSection {
        DashboardSection::at(self.section)
    }

    /// Selected row within the active section.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether a write is waiting for its confirming `Enter`.
    #[must_use]
    pub fn confirm_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Snapshot currently rendered.
    #[must_use]
    pub fn snapshot(&self) -> &DashboardSnapshot {
        &self.snapshot
    }

    /// Selected row, if the section has one at the cursor.
    #[must_use]
    pub fn selected(&self) -> Option<&DashboardRow> {
        let data = self.snapshot.section(self.section());
        if !data.is_ready() {
            return None;
        }
        data.rows().get(self.cursor)
    }

    /// Replace the snapshot, clamping the cursor and disarming any write.
    pub fn set_snapshot(&mut self, snapshot: DashboardSnapshot) {
        self.snapshot = snapshot;
        self.armed = None;
        self.clamp_cursor();
    }

    /// Open the panel (keeps the previous section, resets confirmation).
    pub fn open(&mut self) {
        self.visible = true;
        self.armed = None;
        self.clamp_cursor();
    }

    /// Close the panel. Returns `true` when it was open.
    pub fn close(&mut self) -> bool {
        let was_visible = self.visible;
        self.visible = false;
        self.armed = None;
        was_visible
    }

    /// Move to a section, resetting the cursor and disarming.
    pub fn set_section(&mut self, section: DashboardSection) {
        self.section = section.index();
        self.cursor = 0;
        self.armed = None;
    }

    fn clamp_cursor(&mut self) {
        let len = self.snapshot.section(self.section()).nav_len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.snapshot.section(self.section()).nav_len();
        if len == 0 {
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
        self.disarm();
    }

    fn disarm(&mut self) {
        self.armed = None;
    }

    /// Move one full page using the last painted body height.
    fn page(&self) -> usize {
        // A page is the visible row budget the paint path reported; before
        // the first paint fall back to a conservative screenful.
        self.last_page.max(1)
    }

    fn set_page(&mut self, page: usize) {
        self.last_page = page.clamp(1, DASHBOARD_MAX_ROWS);
    }
}

/// Resolved panel geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DashboardLayout {
    /// Left column (view cells).
    pub x: u16,
    /// Top row (view cells).
    pub y: u16,
    /// Panel width including the 1-cell border.
    pub w: u16,
    /// Panel height including border, title, tabs, and footer.
    pub h: u16,
    /// Rows that fit the body.
    pub visible_rows: usize,
}

/// Resolve the near-full-panel geometry.
///
/// Returns `None` when the view cannot hold a legible panel; the state is
/// kept so a later resize paints it.
pub(super) fn dashboard_layout(view_w: u16, view_h: u16) -> Option<DashboardLayout> {
    if view_w < DASHBOARD_MIN_VIEW_W || view_h < DASHBOARD_MIN_VIEW_H {
        return None;
    }
    let w = view_w - 2;
    let h = view_h - 2;
    // Border top/bottom (2) + title (1) + tabs (1) + footer (1).
    let body = usize::from(h).saturating_sub(5);
    if body == 0 {
        return None;
    }
    Some(DashboardLayout {
        x: 1,
        y: 1,
        w,
        h,
        visible_rows: body,
    })
}

/// Truncate `text` to at most `max` bytes on a char boundary, marking the
/// cut with an ellipsis. Never splits a UTF-8 sequence.
#[must_use]
pub fn bounded_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    // The ellipsis itself costs three bytes; without room for it, cut plain.
    if max < '\u{2026}'.len_utf8() {
        let mut cut = max;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        return text[..cut].to_string();
    }
    let mut cut = max - '\u{2026}'.len_utf8();
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text[..cut].to_string();
    out.push('\u{2026}');
    out
}

impl Runtime {
    /// Whether the configuration dashboard is open.
    #[must_use]
    pub fn dashboard_visible(&self) -> bool {
        self.dashboard.visible()
    }

    /// Flip dashboard visibility. Returns the new visibility.
    ///
    /// Opening never fails; data freshness is the app's concern (it builds
    /// the snapshot before opening). Forces a repaint either way.
    pub fn toggle_dashboard(&mut self) -> bool {
        if self.dashboard.visible() {
            self.dashboard.close();
        } else {
            self.dashboard.open();
        }
        self.pending_full_redraw = true;
        self.dashboard.visible()
    }

    /// Close the dashboard without further effect.
    ///
    /// Returns `true` when it was open (something was dismissed).
    pub fn dismiss_dashboard(&mut self) -> bool {
        if !self.dashboard.visible() {
            return false;
        }
        self.dashboard.close();
        self.pending_full_redraw = true;
        true
    }

    /// Replace the rendered snapshot (cold path: open, `r`, or after a write).
    pub fn set_dashboard_snapshot(&mut self, snapshot: DashboardSnapshot) {
        self.dashboard.set_snapshot(snapshot);
        if self.dashboard.visible() {
            self.pending_full_redraw = true;
        }
    }

    /// Current snapshot (diagnostics/tests).
    #[must_use]
    pub fn dashboard_snapshot(&self) -> &DashboardSnapshot {
        self.dashboard.snapshot()
    }

    /// Active section.
    #[must_use]
    pub fn dashboard_section(&self) -> DashboardSection {
        self.dashboard.section()
    }

    /// Selected row index within the active section.
    #[must_use]
    pub fn dashboard_cursor(&self) -> usize {
        self.dashboard.cursor()
    }

    /// Selected row, if any.
    #[must_use]
    pub fn dashboard_selected(&self) -> Option<&DashboardRow> {
        self.dashboard.selected()
    }

    /// Whether a write waits for its confirming `Enter`.
    #[must_use]
    pub fn dashboard_confirm_armed(&self) -> bool {
        self.dashboard.confirm_armed()
    }

    /// Feed one mapped keypress into the dashboard.
    ///
    /// Pure state transition + typed intent: no I/O, no VM, no capability.
    /// The app maps `j`/`k`/`h`/`l`/`1..=5`/`Enter`/`r`/`q`/`Esc` and their
    /// named-key equivalents, then applies [`DashboardOutcome::Activate`].
    pub fn dashboard_key(&mut self, key: DashboardKey) -> DashboardOutcome {
        let outcome = self.dashboard_key_inner(key);
        if !matches!(outcome, DashboardOutcome::Ignored) {
            // Every state transition is a present-layer change; the frame-on-
            // demand tick must present it even with no PTY damage.
            self.pending_full_redraw = true;
        }
        outcome
    }

    fn dashboard_key_inner(&mut self, key: DashboardKey) -> DashboardOutcome {
        if !self.dashboard.visible() {
            return DashboardOutcome::Ignored;
        }
        match key {
            DashboardKey::Up => {
                self.dashboard.move_cursor(-1);
                DashboardOutcome::Redraw
            }
            DashboardKey::Down => {
                self.dashboard.move_cursor(1);
                DashboardOutcome::Redraw
            }
            DashboardKey::PageUp => {
                let page = self.dashboard.page() as isize;
                self.dashboard.move_cursor(-page);
                DashboardOutcome::Redraw
            }
            DashboardKey::PageDown => {
                let page = self.dashboard.page() as isize;
                self.dashboard.move_cursor(page);
                DashboardOutcome::Redraw
            }
            DashboardKey::First => {
                self.dashboard.cursor = 0;
                self.dashboard.disarm();
                DashboardOutcome::Redraw
            }
            DashboardKey::Last => {
                let len = self
                    .dashboard
                    .snapshot
                    .section(self.dashboard.section())
                    .nav_len();
                self.dashboard.cursor = len.saturating_sub(1);
                self.dashboard.disarm();
                DashboardOutcome::Redraw
            }
            DashboardKey::NextSection => {
                let next = (self.dashboard.section + 1) % DashboardSection::ALL.len();
                self.dashboard.set_section(DashboardSection::at(next));
                DashboardOutcome::Redraw
            }
            DashboardKey::PrevSection => {
                let len = DashboardSection::ALL.len();
                let prev = (self.dashboard.section + len - 1) % len;
                self.dashboard.set_section(DashboardSection::at(prev));
                DashboardOutcome::Redraw
            }
            DashboardKey::Jump(index) => {
                let wanted = usize::from(index);
                if !(1..=DashboardSection::ALL.len()).contains(&wanted) {
                    return DashboardOutcome::Ignored;
                }
                self.dashboard.set_section(DashboardSection::at(wanted - 1));
                DashboardOutcome::Redraw
            }
            DashboardKey::Activate => self.dashboard_activate(),
            DashboardKey::Reload => DashboardOutcome::Reload,
            DashboardKey::Close => {
                if self.dashboard.close() {
                    self.pending_full_redraw = true;
                    DashboardOutcome::Close
                } else {
                    DashboardOutcome::Ignored
                }
            }
        }
    }

    /// Activation step: arm a write, or return the confirmed intent.
    fn dashboard_activate(&mut self) -> DashboardOutcome {
        let Some(row) = self.dashboard.selected() else {
            return DashboardOutcome::Ignored;
        };
        if !row.is_actionable() {
            return DashboardOutcome::Ignored;
        }
        let action = row.action.clone();
        let armed = ArmedConfirm {
            section: self.dashboard.section,
            row: self.dashboard.cursor,
        };
        if self.dashboard.armed == Some(armed) {
            self.dashboard.disarm();
            DashboardOutcome::Activate(action)
        } else {
            self.dashboard.armed = Some(armed);
            DashboardOutcome::Armed
        }
    }

    /// Live-apply a built-in theme preset without persisting it.
    ///
    /// Mirrors the `bitty.theme.preview` live path (`set_theme_palette`,
    /// no durable write). An unknown or non-preset selection is refused
    /// with the palette untouched, so a stale dashboard row can never
    /// half-apply a configuration.
    ///
    /// # Errors
    ///
    /// The `bitty-config` refusal message for a name outside the preset
    /// registry.
    pub fn preview_theme_preset(&mut self, name: &str) -> Result<bool, String> {
        // A stale or hostile row must never half-apply: an unknown preset
        // (or a dual whose halves are not both known) is refused with the
        // palette untouched. A dual previews its dark half, the same
        // dark-first default startup applies.
        let (theme, resolution) =
            bitty_config::theme::resolve_selection_with_status(Some(name), false);
        if resolution == bitty_config::theme::ThemeResolution::FallbackUnknown {
            return Err(format!("unknown theme preset '{name}'"));
        }
        let before = self.config.theme;
        self.set_theme_palette(crate::ThemePalette::from_theme(theme));
        Ok(before != self.config.theme)
    }

    /// Paint the dashboard onto the present-layer overlay.
    ///
    /// Near-full-panel geometry on the focused view: border, background,
    /// title, tab strip, one glyph run per visible row (with the selected
    /// row highlighted), an optional `+N more` tail, and the footer.
    /// Overlay only: pushes fills/glyphs, never mutates cells, scrollback,
    /// layout, or focus. Returns `true` when anything was pushed.
    pub(super) fn paint_dashboard_panel(
        &mut self,
        frames: &[super::layout_focus::PresentFrame],
        view_map: &std::collections::HashMap<ViewId, View>,
        pad_px: i32,
        fills: &mut Vec<bitty_render::grid::FillRect>,
        glyphs: &mut Vec<bitty_render::grid::GlyphInstance>,
    ) -> bool {
        if !self.dashboard.visible() {
            return false;
        }
        let Some(fid) = self.focused_view().or(view_map.keys().next().copied()) else {
            return false;
        };
        let Some(frame) = frames.iter().find(|frame| frame.view == fid) else {
            return false;
        };
        if frame.cols == 0 || frame.rows == 0 {
            return false;
        }
        let Some(panel) = dashboard_layout(frame.cols, frame.rows) else {
            // Fail visible, never silent: keys stay captured while the panel
            // is open, so a too-small view must say why nothing rendered.
            use super::present::px_add;
            let origin_x = px_add(pad_px, frame.content.x);
            let origin_y = px_add(pad_px, frame.content.y);
            let glyphs_at = self.renderer.overlay_text_glyphs(
                DASHBOARD_TOO_SMALL_HINT,
                (origin_x, origin_y),
                usize::from(frame.cols),
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(glyphs_at);
            return true;
        };
        let live = self.live_cell_metrics();
        use super::present::{px_add, px_offset_cells, px_side, px_span};
        let cw = px_side(live.width);
        let ch = px_side(live.height);
        let origin_px_x = px_add(pad_px, frame.content.x);
        let origin_px_y = px_add(pad_px, frame.content.y);
        let panel_px_x = px_offset_cells(origin_px_x, panel.x, live.width);
        let panel_px_y = px_offset_cells(origin_px_y, panel.y, live.height);
        let panel_px_w = px_span(panel.w, live.width);
        let panel_px_h = px_span(panel.h, live.height);
        let panel_rect =
            bitty_render::geometry::RectPx::new(panel_px_x, panel_px_y, panel_px_w, panel_px_h);
        // Border, then the inset background leaves a 1-cell outline.
        fills.push(bitty_render::grid::FillRect {
            rect: panel_rect,
            color: bitty_render::grid::HELP_PANEL_BORDER,
        });
        fills.push(bitty_render::grid::FillRect {
            rect: bitty_render::geometry::RectPx::new(
                px_add(panel_px_x, cw),
                px_add(panel_px_y, ch),
                panel_px_w.saturating_sub(live.width.saturating_mul(2)),
                panel_px_h.saturating_sub(live.height.saturating_mul(2)),
            ),
            color: bitty_render::grid::HELP_PANEL_BG,
        });
        // Same opaque-panel rule as the help popup (CTX-0336): remember
        // where the panel glyphs start, then drop pre-existing glyphs that
        // cross the panel frame so the surface reads as its own background.
        let occlude_start = glyphs.len();
        let inner_cells = usize::from(panel.w.saturating_sub(2));
        let text_x = px_add(panel_px_x, cw);
        let text_y = |row: i32| {
            px_offset_cells(
                panel_px_y,
                u16::try_from(row).unwrap_or(u16::MAX),
                live.height,
            )
        };
        let title = self.renderer.overlay_text_glyphs(
            DASHBOARD_TITLE,
            (text_x, text_y(1)),
            inner_cells,
            bitty_render::grid::HELP_PANEL_FG,
        );
        glyphs.extend(title);
        // Tab strip: `[Overview] Plugins Settings ...`.
        let tabs: Vec<String> = DashboardSection::ALL
            .iter()
            .map(|section| {
                if *section == self.dashboard.section() {
                    format!("[{}]", section.title())
                } else {
                    format!(" {} ", section.title())
                }
            })
            .collect();
        let tabs_line = bounded_text(&tabs.join(" "), inner_cells);
        let tab_glyphs = self.renderer.overlay_text_glyphs(
            &tabs_line,
            (text_x, text_y(2)),
            inner_cells,
            bitty_render::grid::HELP_PANEL_FG,
        );
        glyphs.extend(tab_glyphs);
        let body = panel.visible_rows;
        self.dashboard.set_page(body);
        let data = self.dashboard.snapshot().section(self.dashboard.section());
        let rows = data.rows();
        let (start, visible, overflow) = if data.is_ready() {
            if rows.is_empty() {
                (0, 0, 0)
            } else {
                let visible = rows
                    .len()
                    .min(body.saturating_sub(usize::from(rows.len() > body)));
                let start = self
                    .dashboard
                    .cursor()
                    .saturating_sub(visible.saturating_sub(1))
                    .min(rows.len().saturating_sub(visible));
                (start, visible, rows.len() - visible)
            }
        } else {
            (0, 0, 0)
        };
        // Column widths come from the visible window only, so scrolling
        // never reflows the label column mid-interaction.
        let label_width = rows
            .iter()
            .skip(start)
            .take(visible)
            .map(|row| row.label.chars().count())
            .max()
            .unwrap_or(0)
            .min(inner_cells.saturating_sub(4));
        let mut painted = 0usize;
        for (offset, row) in rows.iter().skip(start).take(visible).enumerate() {
            let index = start + offset;
            let row_y = text_y(3 + i32::try_from(offset).unwrap_or(i32::MAX));
            let label_pad = label_width.saturating_sub(row.label.chars().count());
            let mut line = String::with_capacity(inner_cells);
            // ASCII selection marker: the row fill is subtle under high
            // contrast and monochrome themes, the marker is not.
            line.push_str(if index == self.dashboard.cursor() {
                "> "
            } else {
                "  "
            });
            line.push_str(&row.label);
            for _ in 0..label_pad.saturating_add(2) {
                line.push(' ');
            }
            line.push_str(&row.value);
            let line = bounded_text(&line, inner_cells);
            if index == self.dashboard.cursor() {
                fills.push(bitty_render::grid::FillRect {
                    rect: bitty_render::geometry::RectPx::new(
                        text_x,
                        row_y,
                        px_span(u16::try_from(inner_cells).unwrap_or(u16::MAX), live.width),
                        live.height,
                    ),
                    color: self.config.theme.selection,
                });
            }
            let row_glyphs = self.renderer.overlay_text_glyphs(
                &line,
                (text_x, row_y),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(row_glyphs);
            painted += 1;
        }
        if !data.is_ready() {
            let reason = match data {
                DashboardSectionData::Unavailable { reason } => reason.clone(),
                DashboardSectionData::Ready(_) => String::new(),
            };
            let line = bounded_text(&format!("unavailable: {reason}"), inner_cells);
            let row_glyphs = self.renderer.overlay_text_glyphs(
                &line,
                (text_x, text_y(3)),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(row_glyphs);
            painted = 1;
        } else if rows.is_empty() {
            let line = bounded_text("(empty)", inner_cells);
            let row_glyphs = self.renderer.overlay_text_glyphs(
                &line,
                (text_x, text_y(3)),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(row_glyphs);
            painted = 1;
        }
        let mut footer_row = 3 + i32::try_from(painted).unwrap_or(i32::MAX);
        if overflow > 0 {
            let tail = bounded_text(&format!("+{overflow} more"), inner_cells);
            let tail_glyphs = self.renderer.overlay_text_glyphs(
                &tail,
                (text_x, text_y(footer_row)),
                inner_cells,
                bitty_render::grid::HELP_PANEL_FG,
            );
            glyphs.extend(tail_glyphs);
            footer_row = footer_row.saturating_add(1);
        }
        // Keep the footer on the last inner row regardless of body fill so
        // the hint never scrolls away.
        let footer_row = footer_row.max(i32::from(panel.h) - 3);
        let footer = if self.dashboard.confirm_armed() {
            DASHBOARD_CONFIRM_HINT
        } else {
            DASHBOARD_FOOTER_HINT
        };
        let footer = bounded_text(footer, inner_cells);
        let footer_glyphs = self.renderer.overlay_text_glyphs(
            &footer,
            (text_x, text_y(footer_row)),
            inner_cells,
            bitty_render::grid::HELP_PANEL_FG,
        );
        glyphs.extend(footer_glyphs);
        let panel_glyphs = glyphs.split_off(occlude_start);
        glyphs.retain(|glyph| !glyph_overlaps_rect(glyph, panel_rect));
        glyphs.extend(panel_glyphs);
        true
    }
}

/// Re-export of the help popup's overlap predicate so both opaque overlays
/// share one implementation.
use super::help::glyph_overlaps_rect;

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, value: &str) -> DashboardRow {
        DashboardRow::info(label, value)
    }

    fn snapshot_with(section: DashboardSection, data: DashboardSectionData) -> DashboardSnapshot {
        let mut snapshot = DashboardSnapshot::empty();
        snapshot.set_section(section, data);
        snapshot
    }

    #[test]
    fn sections_are_closed_and_ordered() {
        assert_eq!(DashboardSection::ALL.len(), 6);
        for (index, section) in DashboardSection::ALL.iter().enumerate() {
            assert_eq!(section.index(), index);
            assert_eq!(DashboardSection::at(index), *section);
        }
        assert_eq!(DashboardSection::at(99), DashboardSection::Diagnostics);
    }

    #[test]
    fn bounded_text_never_splits_utf8() {
        let text = "日本語のラベル";
        let bounded = bounded_text(text, 7);
        assert!(bounded.len() <= 7);
        assert!(bounded.ends_with('\u{2026}'));
        assert!(bounded.chars().count() < text.chars().count());
        assert_eq!(bounded_text("short", 16), "short");
    }

    #[test]
    fn ready_section_caps_rows_and_reports_overflow() {
        let rows: Vec<DashboardRow> = (0..DASHBOARD_MAX_ROWS + 5)
            .map(|index| row(&format!("row-{index}"), ""))
            .collect();
        let data = DashboardSectionData::ready(rows);
        let rows = data.rows();
        assert_eq!(rows.len(), DASHBOARD_MAX_ROWS);
        assert_eq!(rows.last().unwrap().label, DASHBOARD_OVERFLOW_LABEL);
        assert!(rows.last().unwrap().action == DashboardAction::None);
        assert!(rows.last().unwrap().value.contains("6 more rows"));
    }

    #[test]
    fn row_fields_are_bounded() {
        let long = "x".repeat(4096);
        let row = DashboardRow::info(long.clone(), long.clone());
        assert!(row.label.len() <= DASHBOARD_MAX_LABEL_BYTES);
        assert!(row.value.len() <= DASHBOARD_MAX_VALUE_BYTES);
    }

    #[test]
    fn path_rows_keep_long_host_paths() {
        // Regression: macOS temp roots (`/var/folders/...`) exceed the normal
        // 96-byte value bound, which used to cut the `(missing)` marker and
        // the extension executable name out of the displayed row.
        let long = format!(
            "/var/folders/xx/yyy/T/{}/init.lua (missing)",
            "d".repeat(120)
        );
        let row = DashboardRow::path("config", &long);
        assert_eq!(row.value, long);
        assert!(row.value.ends_with("(missing)"));
        let oversized = format!("/{}", "p".repeat(400));
        let row = DashboardRow::path("config", oversized);
        assert!(row.value.len() <= DASHBOARD_MAX_PATH_BYTES);
        assert!(row.value.ends_with('\u{2026}'));
        // Ordinary values keep the tighter bound.
        let row = DashboardRow::info("theme", "t".repeat(200));
        assert!(row.value.len() <= DASHBOARD_MAX_VALUE_BYTES);
    }

    #[test]
    fn unavailable_section_is_not_ready_and_has_no_rows() {
        let data = DashboardSectionData::unavailable("config root missing");
        assert!(!data.is_ready());
        assert!(data.rows().is_empty());
        assert_eq!(data.nav_len(), 1);
    }

    #[test]
    fn nav_len_tracks_rows() {
        let data = DashboardSectionData::ready(vec![row("a", ""), row("b", "")]);
        assert_eq!(data.nav_len(), 2);
        assert_eq!(DashboardSectionData::ready(Vec::new()).nav_len(), 0);
    }

    #[test]
    fn layout_is_full_panel_with_one_cell_margin() {
        let layout = dashboard_layout(100, 40).expect("fits");
        assert_eq!((layout.x, layout.y), (1, 1));
        assert_eq!((layout.w, layout.h), (98, 38));
        assert_eq!(layout.visible_rows, 33);
        assert!(dashboard_layout(20, 40).is_none());
        assert!(dashboard_layout(100, 6).is_none());
    }

    #[test]
    fn empty_snapshot_reports_no_data() {
        assert!(!DashboardSnapshot::empty().has_data());
        let snapshot = snapshot_with(
            DashboardSection::Overview,
            DashboardSectionData::ready(vec![row("theme", "bitty-dark")]),
        );
        assert!(snapshot.has_data());
    }

    #[test]
    fn state_open_close_is_idempotent() {
        let mut state = DashboardState::default();
        assert!(!state.visible());
        state.open();
        assert!(state.visible());
        assert!(state.close());
        assert!(!state.close());
        assert!(!state.visible());
        assert!(!state.confirm_armed());
    }

    #[test]
    fn cursor_moves_clamp_and_disarm() {
        let mut state = DashboardState::default();
        state.set_snapshot(snapshot_with(
            DashboardSection::Plugins,
            DashboardSectionData::ready(vec![row("a", ""), row("b", "")]),
        ));
        state.set_section(DashboardSection::Plugins);
        state.open();
        assert_eq!(state.cursor(), 0);
        state.move_cursor(-1);
        assert_eq!(state.cursor(), 0);
        state.move_cursor(1);
        assert_eq!(state.cursor(), 1);
        state.move_cursor(1);
        assert_eq!(state.cursor(), 1);
        state.armed = Some(ArmedConfirm { section: 1, row: 1 });
        state.move_cursor(-1);
        assert!(!state.confirm_armed());
    }

    #[test]
    fn section_change_resets_cursor_and_disarms() {
        let mut state = DashboardState::default();
        state.set_snapshot(snapshot_with(
            DashboardSection::Overview,
            DashboardSectionData::ready(vec![row("a", ""), row("b", ""), row("c", "")]),
        ));
        state.open();
        state.move_cursor(2);
        state.armed = Some(ArmedConfirm { section: 0, row: 2 });
        state.set_section(DashboardSection::Extensions);
        assert_eq!(state.section(), DashboardSection::Extensions);
        assert_eq!(state.cursor(), 0);
        assert!(!state.confirm_armed());
    }

    #[test]
    fn selected_row_is_none_for_unavailable_and_empty_sections() {
        let mut state = DashboardState::default();
        state.set_snapshot(snapshot_with(
            DashboardSection::Settings,
            DashboardSectionData::unavailable("no plugin selected"),
        ));
        state.open();
        assert!(state.selected().is_none());

        let mut state = DashboardState::default();
        state.set_snapshot(snapshot_with(
            DashboardSection::Profiles,
            DashboardSectionData::ready(Vec::new()),
        ));
        state.open();
        assert!(state.selected().is_none());
    }

    fn runtime_with_dashboard() -> Runtime {
        Runtime::with_defaults().expect("headless runtime builds")
    }

    fn action_snapshot() -> DashboardSnapshot {
        let mut snapshot = DashboardSnapshot::empty();
        snapshot.set_section(
            DashboardSection::Plugins,
            DashboardSectionData::ready(vec![
                DashboardRow::info("bitty-terminal.shell-integration", "available"),
                DashboardRow::action(
                    "bitty-terminal.palette",
                    "enabled",
                    DashboardAction::SetPluginEnabled {
                        id: "bitty-terminal.palette".to_string(),
                        enabled: true,
                    },
                ),
            ]),
        );
        snapshot
    }

    #[test]
    fn activation_needs_a_confirming_second_press() {
        let mut runtime = runtime_with_dashboard();
        runtime.set_dashboard_snapshot(action_snapshot());
        runtime.toggle_dashboard();
        runtime.dashboard_key(DashboardKey::Jump(2));
        // Read-only row: no write is ever armed.
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Ignored
        );
        assert!(!runtime.dashboard_confirm_armed());
        // Actionable row: first press arms, second confirms.
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Down),
            DashboardOutcome::Redraw
        );
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Armed
        );
        assert!(runtime.dashboard_confirm_armed());
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Activate(DashboardAction::SetPluginEnabled {
                id: "bitty-terminal.palette".to_string(),
                enabled: true,
            })
        );
        assert!(!runtime.dashboard_confirm_armed());
    }

    #[test]
    fn moving_after_arming_cancels_the_write() {
        let mut runtime = runtime_with_dashboard();
        runtime.set_dashboard_snapshot(action_snapshot());
        runtime.toggle_dashboard();
        runtime.dashboard_key(DashboardKey::Jump(2));
        runtime.dashboard_key(DashboardKey::Down);
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Armed
        );
        runtime.dashboard_key(DashboardKey::Up);
        assert!(!runtime.dashboard_confirm_armed());
        // The confirming press now re-arms instead of writing.
        runtime.dashboard_key(DashboardKey::Down);
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Armed
        );
    }

    #[test]
    fn section_keys_are_bounded_and_reload_is_reported() {
        let mut runtime = runtime_with_dashboard();
        runtime.set_dashboard_snapshot(action_snapshot());
        runtime.toggle_dashboard();
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Jump(3)),
            DashboardOutcome::Redraw
        );
        assert_eq!(runtime.dashboard_section(), DashboardSection::Settings);
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Jump(0)),
            DashboardOutcome::Ignored
        );
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Jump(6)),
            DashboardOutcome::Redraw
        );
        assert_eq!(runtime.dashboard_section(), DashboardSection::Diagnostics);
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Jump(9)),
            DashboardOutcome::Ignored
        );
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Reload),
            DashboardOutcome::Reload
        );
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Close),
            DashboardOutcome::Close
        );
        assert!(!runtime.dashboard_visible());
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Down),
            DashboardOutcome::Ignored
        );
    }

    #[test]
    fn theme_preview_applies_live_and_rejects_unknown_names() {
        let mut runtime = runtime_with_dashboard();
        let before = runtime.config().theme.background;
        let other = bitty_config::theme::ALL_PRESETS
            .iter()
            .find(|theme| theme.name != bitty_config::theme::DEFAULT_THEME_NAME)
            .expect("catalog has more than one preset");
        runtime
            .preview_theme_preset(other.name)
            .expect("known preset previews");
        assert_ne!(runtime.config().theme.background, before);
        assert!(runtime.preview_theme_preset("not-a-preset").is_err());
    }

    #[test]
    fn settings_and_extensions_rows_are_never_actionable() {
        let mut snapshot = DashboardSnapshot::empty();
        snapshot.set_section(
            DashboardSection::Settings,
            DashboardSectionData::ready(vec![DashboardRow::info("search:row_count", "10")]),
        );
        snapshot.set_section(
            DashboardSection::Extensions,
            DashboardSectionData::ready(vec![DashboardRow::info("net", "0.0.1 active (user)")]),
        );
        let mut runtime = runtime_with_dashboard();
        runtime.set_dashboard_snapshot(snapshot);
        runtime.toggle_dashboard();
        runtime.dashboard_key(DashboardKey::Jump(3));
        assert!(runtime.dashboard_selected().is_some());
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Ignored,
            "the action enum cannot target settings rows"
        );
        runtime.dashboard_key(DashboardKey::Jump(5));
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Activate),
            DashboardOutcome::Ignored,
            "the action enum cannot target extension rows"
        );
    }

    #[test]
    fn diagnostics_section_is_part_of_the_closed_set() {
        let mut runtime = runtime_with_dashboard();
        runtime.toggle_dashboard();
        assert_eq!(
            runtime.dashboard_key(DashboardKey::Jump(6)),
            DashboardOutcome::Redraw
        );
        assert_eq!(runtime.dashboard_section(), DashboardSection::Diagnostics);
        assert!(
            !runtime
                .dashboard_snapshot()
                .section(DashboardSection::Diagnostics)
                .is_ready()
        );
    }

    #[test]
    fn too_small_hint_is_defined_and_layout_refuses() {
        assert!(!DASHBOARD_TOO_SMALL_HINT.is_empty());
        assert!(dashboard_layout(29, 9).is_none());
        assert!(dashboard_layout(80, 8).is_none());
    }

    #[test]
    fn paint_requires_an_attached_frame_and_visible_state() {
        // The overlay must never panic with no allocation, no glyph cache, or
        // an unavailable view: it simply paints nothing and the state stays
        // visible for a later resize.
        let mut runtime = runtime_with_dashboard();
        runtime.set_dashboard_snapshot(action_snapshot());
        assert!(!runtime.dashboard_visible());
        runtime.toggle_dashboard();
        assert!(runtime.dashboard_visible());
        let map = std::collections::HashMap::new();
        let mut fills = Vec::new();
        let mut glyphs = Vec::new();
        assert!(!runtime.paint_dashboard_panel(&[], &map, 0, &mut fills, &mut glyphs));
        assert!(fills.is_empty() && glyphs.is_empty());
        assert!(runtime.dashboard_visible());
    }

    #[test]
    fn snapshot_replacement_clamps_cursor() {
        let mut state = DashboardState::default();
        state.set_snapshot(snapshot_with(
            DashboardSection::Plugins,
            DashboardSectionData::ready(vec![row("a", ""), row("b", ""), row("c", "")]),
        ));
        state.set_section(DashboardSection::Plugins);
        state.open();
        state.move_cursor(2);
        state.set_snapshot(snapshot_with(
            DashboardSection::Plugins,
            DashboardSectionData::ready(vec![row("only", "")]),
        ));
        assert_eq!(state.cursor(), 0);
        state.set_snapshot(snapshot_with(
            DashboardSection::Plugins,
            DashboardSectionData::unavailable("store unreadable"),
        ));
        assert_eq!(state.cursor(), 0);
    }
}
