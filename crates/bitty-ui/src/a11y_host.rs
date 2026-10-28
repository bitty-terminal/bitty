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
//!   Notices are one-shot: the next [`bitty_a11y::Adapter::ingest_host`] read
//!   reports every pending notice once and clears it, so a reused host never
//!   re-delivers a transition (#2352).
//! - [`A11ySettings`] — core-owned reduced-motion plus high-contrast
//!   composition over [`MotionSpec`] and theme
//!   tokens. The extension crate is dependency-free and cannot own these; they
//!   stay here by construction and never take plugin input.
//!
//! Bounded inputs (fail-closed, never silently truncated, except the
//! announcement queue's documented drop-oldest overflow):
//!
//! - Names, labels, chrome state: at most 256 characters
//!   ([`MAX_ACCESSIBLE_NAME_LEN`]).
//! - Nodes per snapshot: at most 4096
//!   ([`MAX_SNAPSHOT_NODES`]).
//! - Painted scene text: at most [`MAX_SCENE_TEXT_LINES`] lines and
//!   [`MAX_SCENE_TEXT_BYTES`] bytes per host (CTX-1628).
//! - Announcement text: at most 256 characters
//!   ([`MAX_ANNOUNCEMENT_TEXT_LEN`]).
//! - Pending announcements: at most 8
//!   ([`MAX_PENDING_ANNOUNCEMENTS`]).
//!   A full queue drops the oldest notice so the newest state wins (W-134);
//!   overflow does not fail the ingest. The host-side queue is bounded on
//!   [`UiSnapshotHost::push_notice`], so an untouched host never grows past
//!   the documented ceiling.
//!
//! The projection is derived and read-only: every constructor takes shared
//! references and returns owned data. It never mutates terminal state, is
//! never persisted, and is never consulted for routing or dispatch. Removing
//! the adapter removes platform exposure only; this host keeps working with
//! zero backends and in safe mode.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use bitty_a11y::{
    A11yError, AnnouncementKind, ChromeKind, CursorPosition, FocusOwner, HandleMap,
    InteractivePurpose, MAX_ACCESSIBLE_NAME_LEN, MAX_ANNOUNCEMENT_TEXT_LEN,
    MAX_PENDING_ANNOUNCEMENTS, MAX_SNAPSHOT_NODES, Snapshot, SnapshotBuilder, SnapshotHost,
    TextRun,
};
// Re-exported so Core callers (the runtime scene projection) can name the
// extension scene vocabulary through this module only; `bitty_a11y` stays
// named in exactly one Core source file (W-147 fence).
pub use bitty_a11y::SceneKind;
use bitty_a11y_component::component::protocol::{
    BOUNDS_UNIT_CHARS, CAPABILITY_MASK, MAX_GRID_COLS, MAX_GRID_ROWS, MAX_TREE_DEPTH,
    PROTOCOL_VERSION, ROLE_NONE, STATE_FOCUSED, STATE_MASK, SURFACE_NONE, SURFACE_OVERLAY,
};
use bitty_a11y_component::component::{
    CAPABILITY_DIFF, CAPABILITY_SELECTION, CAPABILITY_SNAPSHOT, ErrorKind,
    HELLO_CAPABILITY_GRANTED, Message, Selection, TreeRow,
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

const MAX_COMPONENT_NAME_CHARS: usize = 256;
const MAX_COMPONENT_TEXT_BYTES: usize = 1024;
const MAX_COMPONENT_ROW_BYTES: usize = 8192;
const MAX_COMPONENT_PROJECTION_BYTES: usize = 1024 * 1024;
const MAX_COMPONENT_NODES: usize = 4096;
const MAX_COMPONENT_PAGES: u32 = 256;
const DEFAULT_COMPONENT_DEADLINE: Duration = Duration::from_millis(250);

/// Maximum painted scene text lines one host retains (CTX-1628).
///
/// Reuses the extension's snapshot node ceiling: a scene never contributes
/// more text rows than the whole accessibility tree may hold nodes, so the
/// projection needs no new numeric ceiling.
pub const MAX_SCENE_TEXT_LINES: usize = MAX_SNAPSHOT_NODES;

/// Maximum painted scene text bytes one host retains (CTX-1628).
///
/// Reuses the bounded component projection ceiling so scene text and a
/// component projection cannot diverge in size.
pub const MAX_SCENE_TEXT_BYTES: usize = MAX_COMPONENT_PROJECTION_BYTES;

/// Default-off component host configuration.
///
/// The Core composition root must explicitly construct this value after its
/// installer, capability authority, and safe-mode policy have made the
/// decision to expose accessibility. This module does not spawn processes,
/// open files, select a platform backend, or grant the capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentHostConfig {
    /// Whether the Core process is in safe mode.
    pub safe_mode: bool,
    /// Whether the Core policy granted component accessibility projection.
    pub capability_granted: bool,
    /// Maximum time allowed for one handshake or snapshot exchange.
    pub deadline: Duration,
}

impl Default for ComponentHostConfig {
    fn default() -> Self {
        Self {
            safe_mode: true,
            capability_granted: false,
            deadline: DEFAULT_COMPONENT_DEADLINE,
        }
    }
}

/// Errors returned by the default-off component host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentHostError {
    /// Safe mode or missing capability withheld the projection.
    Denied,
    /// The peer returned a typed error.
    Peer(ErrorKind),
    /// The stream or component ended before a complete exchange.
    ComponentLost,
    /// The exchange exceeded its caller-supplied deadline.
    Deadline,
    /// The peer violated the protocol or the bounded tree contract.
    Invalid(String),
}

impl fmt::Display for ComponentHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("accessibility component denied"),
            Self::Peer(kind) => write!(f, "accessibility component error: {kind}"),
            Self::ComponentLost => f.write_str("accessibility component lost"),
            Self::Deadline => f.write_str("accessibility component deadline exceeded"),
            Self::Invalid(reason) => {
                write!(f, "invalid accessibility component response: {reason}")
            }
        }
    }
}

impl std::error::Error for ComponentHostError {}

/// A complete, validated component projection.
///
/// This is read-only presentation data. Stable ids are retained only as
/// projection data; no id is converted into a Core action target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentProjection {
    generation: u64,
    rows: Vec<TreeRow>,
    focus: Option<u64>,
    selection: Option<Selection>,
}

impl ComponentProjection {
    /// Projection generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Validated rows in deterministic wire order.
    #[must_use]
    pub fn rows(&self) -> &[TreeRow] {
        &self.rows
    }

    /// Stable focused id, if the component reported one.
    #[must_use]
    pub const fn focus(&self) -> Option<u64> {
        self.focus
    }

    /// Selection state, if the component reported one.
    #[must_use]
    pub const fn selection(&self) -> Option<Selection> {
        self.selection
    }
}

/// Transport boundary owned by the Core composition root.
///
/// Implementations must enforce their own I/O deadline. The host never
/// performs blocking I/O on the UI thread and never assumes that EOF means a
/// valid empty projection: EOF is always a fail-closed component loss.
pub trait ComponentTransport {
    /// Writes one complete framed message.
    fn send(&mut self, message: &Message) -> io::Result<()>;
    /// Reads one complete framed message before `deadline`.
    fn receive(&mut self, deadline: Instant) -> io::Result<Option<Message>>;
}

/// Validating Core client for the installable `bitty-a11y` component.
///
/// It is intentionally not wired into the default linked [`SnapshotHost`]
/// path. Until a platform backend and an explicit composition-root policy are
/// accepted, the linked model remains the safe fallback and this client is a
/// contract seam only.
#[derive(Debug)]
pub struct ComponentSnapshotHost<T> {
    transport: T,
    config: ComponentHostConfig,
    capabilities: u8,
    projection: Option<ComponentProjection>,
    retired_ids: std::collections::BTreeSet<u64>,
    next_request: u64,
}

impl<T: ComponentTransport> ComponentSnapshotHost<T> {
    /// Completes the capability-gated handshake.
    pub fn connect(
        mut transport: T,
        config: ComponentHostConfig,
    ) -> Result<Self, ComponentHostError> {
        if config.safe_mode || !config.capability_granted || config.deadline.is_zero() {
            return Err(ComponentHostError::Denied);
        }
        let mode = HELLO_CAPABILITY_GRANTED;
        transport
            .send(&Message::Hello {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
                component: "a11y".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                mode,
            })
            .map_err(|_| ComponentHostError::ComponentLost)?;
        let deadline = Instant::now()
            .checked_add(config.deadline)
            .ok_or(ComponentHostError::Deadline)?;
        let response = transport
            .receive(deadline)
            .map_err(map_receive_error)?
            .ok_or(ComponentHostError::ComponentLost)?;
        if Instant::now() > deadline {
            return Err(ComponentHostError::Deadline);
        }
        match response {
            Message::HelloAck {
                protocol,
                component,
                capabilities,
                ..
            } if protocol == PROTOCOL_VERSION && component == "a11y" => {
                if capabilities == 0 {
                    return Err(ComponentHostError::Denied);
                }
                if capabilities & !CAPABILITY_MASK != 0 || capabilities & CAPABILITY_SNAPSHOT == 0 {
                    return Err(ComponentHostError::Invalid(
                        "unsupported capability set".to_owned(),
                    ));
                }
                Ok(Self {
                    transport,
                    config,
                    capabilities,
                    projection: None,
                    retired_ids: std::collections::BTreeSet::new(),
                    next_request: 1,
                })
            }
            Message::HelloAck { .. } => {
                Err(ComponentHostError::Invalid("handshake mismatch".to_owned()))
            }
            Message::Error { kind, .. } => Err(ComponentHostError::Peer(kind)),
            _ => Err(ComponentHostError::Invalid(
                "expected handshake acknowledgement".to_owned(),
            )),
        }
    }

    /// Requests and validates a complete snapshot or bounded diff.
    ///
    /// A diff is applied only after every page, id, parent, depth, generation,
    /// focus, and selection invariant passes. Any failure leaves the prior
    /// projection unavailable and returns a fail-closed error.
    pub fn refresh(&mut self) -> Result<&ComponentProjection, ComponentHostError> {
        let result = self.refresh_inner();
        if result.is_err() {
            // A crashed, stale, malformed, or timed-out component must not
            // leave the last tree exposed as if it were current.
            self.projection = None;
        }
        match result {
            Ok(projection) => {
                if let Some(previous) = self.projection.as_ref() {
                    for row in &previous.rows {
                        if !projection.rows.iter().any(|next| next.id == row.id) {
                            self.retired_ids.insert(row.id);
                        }
                    }
                }
                self.projection = Some(projection);
                Ok(self.projection.as_ref().expect("projection inserted"))
            }
            Err(error) => Err(error),
        }
    }

    fn refresh_inner(&mut self) -> Result<ComponentProjection, ComponentHostError> {
        let request = self.mint_request()?;
        let since = self
            .projection
            .as_ref()
            .map_or(0, ComponentProjection::generation);
        self.transport
            .send(&Message::SnapshotRequest { request, since })
            .map_err(map_receive_error)?;
        let deadline = Instant::now()
            .checked_add(self.config.deadline)
            .ok_or(ComponentHostError::Deadline)?;
        let first = self.receive_for(request, deadline)?;
        let is_diff = matches!(first, Message::DiffBegin { .. });
        let candidate = match first {
            Message::SnapshotBegin {
                generation,
                nodes,
                focus,
                selection,
                pages,
                ..
            } => self.read_snapshot(
                request, generation, nodes, focus, selection, pages, deadline,
            )?,
            Message::DiffBegin {
                base,
                generation,
                removed,
                upserts,
                focus,
                selection,
                pages,
                ..
            } => {
                let current = self.projection.clone().ok_or_else(|| {
                    ComponentHostError::Invalid("diff without baseline".to_owned())
                })?;
                if base != current.generation || self.capabilities & CAPABILITY_DIFF == 0 {
                    return Err(ComponentHostError::Peer(ErrorKind::StaleGeneration));
                }
                self.read_diff(
                    request, &current, generation, removed, upserts, focus, selection, pages,
                    deadline,
                )?
            }
            Message::Error { kind, .. } => return Err(ComponentHostError::Peer(kind)),
            _ => {
                return Err(ComponentHostError::Invalid(
                    "unexpected snapshot response".to_owned(),
                ));
            }
        };
        if is_diff {
            self.transport
                .send(&Message::DiffAck {
                    generation: candidate.generation,
                })
                .map_err(map_receive_error)?;
        }
        Ok(candidate)
    }

    /// Current validated projection, if one has been accepted.
    #[must_use]
    pub const fn projection(&self) -> Option<&ComponentProjection> {
        self.projection.as_ref()
    }

    fn mint_request(&mut self) -> Result<u64, ComponentHostError> {
        let request = self.next_request;
        self.next_request = self
            .next_request
            .checked_add(1)
            .ok_or_else(|| ComponentHostError::Invalid("request id exhausted".to_owned()))?;
        Ok(request)
    }

    fn receive_for(
        &mut self,
        request: u64,
        deadline: Instant,
    ) -> Result<Message, ComponentHostError> {
        if Instant::now() >= deadline {
            return Err(ComponentHostError::Deadline);
        }
        let message = self
            .transport
            .receive(deadline)
            .map_err(map_receive_error)?
            .ok_or(ComponentHostError::ComponentLost)?;
        if Instant::now() > deadline {
            return Err(ComponentHostError::Deadline);
        }
        if message.request_id().is_some_and(|id| id != request) {
            return Err(ComponentHostError::Invalid(
                "response request mismatch".to_owned(),
            ));
        }
        Ok(message)
    }

    #[allow(clippy::too_many_arguments)]
    fn read_snapshot(
        &mut self,
        request: u64,
        generation: u64,
        nodes: u32,
        focus: Option<u64>,
        selection: Option<Selection>,
        pages: u32,
        deadline: Instant,
    ) -> Result<ComponentProjection, ComponentHostError> {
        if generation == 0
            || self
                .projection
                .as_ref()
                .is_some_and(|current| generation <= current.generation)
            || nodes == 0
            || pages == 0
            || nodes as usize > MAX_COMPONENT_NODES
            || pages > MAX_COMPONENT_PAGES
        {
            return Err(ComponentHostError::Invalid(
                "invalid snapshot header".to_owned(),
            ));
        }
        let mut rows = Vec::new();
        for index in 0..pages {
            match self.receive_for(request, deadline)? {
                Message::SnapshotPage {
                    index: received,
                    rows: page,
                    last,
                    ..
                } if received == index => {
                    rows.extend(page);
                    if last != (index + 1 == pages) {
                        return Err(ComponentHostError::Invalid(
                            "snapshot page termination".to_owned(),
                        ));
                    }
                }
                _ => {
                    return Err(ComponentHostError::Invalid(
                        "invalid snapshot page".to_owned(),
                    ));
                }
            }
        }
        if rows.len() != nodes as usize {
            return Err(ComponentHostError::Invalid(
                "snapshot node count".to_owned(),
            ));
        }
        self.validate_projection(generation, rows, focus, selection)
    }

    #[allow(clippy::too_many_arguments)]
    fn read_diff(
        &mut self,
        request: u64,
        current: &ComponentProjection,
        generation: u64,
        removed: u32,
        upserts: u32,
        focus: Option<u64>,
        selection: Option<Selection>,
        pages: u32,
        deadline: Instant,
    ) -> Result<ComponentProjection, ComponentHostError> {
        if generation < current.generation
            || generation == current.generation && (removed != 0 || upserts != 0)
            || pages > MAX_COMPONENT_PAGES
            || pages == 0 && (removed != 0 || upserts != 0)
            || removed as usize > MAX_COMPONENT_NODES
            || upserts as usize > MAX_COMPONENT_NODES
        {
            return Err(ComponentHostError::Invalid(
                "invalid diff header".to_owned(),
            ));
        }
        let mut rows = current.rows.clone();
        let mut removed_ids = Vec::new();
        let mut upsert_rows = Vec::new();
        for index in 0..pages {
            match self.receive_for(request, deadline)? {
                Message::DiffPage {
                    index: received,
                    removed: page_removed,
                    upserts: page_upserts,
                    last,
                    ..
                } if received == index => {
                    removed_ids.extend(page_removed);
                    upsert_rows.extend(page_upserts);
                    if last != (index + 1 == pages) {
                        return Err(ComponentHostError::Invalid(
                            "diff page termination".to_owned(),
                        ));
                    }
                }
                _ => return Err(ComponentHostError::Invalid("invalid diff page".to_owned())),
            }
        }
        if removed_ids.len() != removed as usize || upsert_rows.len() != upserts as usize {
            return Err(ComponentHostError::Invalid("diff entry count".to_owned()));
        }
        let current_ids: std::collections::BTreeSet<u64> = rows.iter().map(|row| row.id).collect();
        let removed_set: std::collections::BTreeSet<u64> = removed_ids.iter().copied().collect();
        if removed_set.len() != removed_ids.len()
            || removed_ids.iter().any(|id| !current_ids.contains(id))
        {
            return Err(ComponentHostError::Invalid(
                "unknown or duplicate removed id".to_owned(),
            ));
        }
        let upsert_ids: std::collections::BTreeSet<u64> =
            upsert_rows.iter().map(|row| row.id).collect();
        if upsert_ids.len() != upsert_rows.len() {
            return Err(ComponentHostError::Invalid(
                "duplicate upsert id".to_owned(),
            ));
        }
        if upsert_ids.iter().any(|id| removed_set.contains(id)) {
            return Err(ComponentHostError::Invalid(
                "removed id reused in diff".to_owned(),
            ));
        }
        for id in &removed_ids {
            rows.retain(|row| row.id != *id);
        }
        for row in upsert_rows {
            rows.retain(|existing| existing.id != row.id);
            rows.push(row);
        }
        rows.sort_by_key(|row| row.id);
        let candidate = self.validate_projection(generation, rows, focus, selection)?;
        self.retired_ids.extend(removed_ids);
        Ok(candidate)
    }

    fn validate_projection(
        &mut self,
        generation: u64,
        rows: Vec<TreeRow>,
        focus: Option<u64>,
        selection: Option<Selection>,
    ) -> Result<ComponentProjection, ComponentHostError> {
        if rows.is_empty() || rows.len() > MAX_COMPONENT_NODES {
            return Err(ComponentHostError::Invalid("node bound".to_owned()));
        }
        if rows.iter().any(|row| {
            row.name.as_ref().is_some_and(|value| {
                value.chars().count() > MAX_COMPONENT_NAME_CHARS
                    || value.len() > MAX_COMPONENT_TEXT_BYTES
            }) || row.state_text.as_ref().is_some_and(|value| {
                value.chars().count() > MAX_COMPONENT_NAME_CHARS
                    || value.len() > MAX_COMPONENT_TEXT_BYTES
            }) || row
                .content
                .as_ref()
                .is_some_and(|value| value.len() > MAX_COMPONENT_ROW_BYTES)
        }) {
            return Err(ComponentHostError::Invalid("text byte bound".to_owned()));
        }
        let text_bytes: usize = rows
            .iter()
            .map(|row| {
                row.name.as_ref().map_or(0, String::len)
                    + row.content.as_ref().map_or(0, String::len)
                    + row.state_text.as_ref().map_or(0, String::len)
            })
            .sum();
        if text_bytes > MAX_COMPONENT_PROJECTION_BYTES {
            return Err(ComponentHostError::Invalid(
                "projection byte bound".to_owned(),
            ));
        }
        let ids: std::collections::BTreeSet<u64> = rows.iter().map(|row| row.id).collect();
        if ids.len() != rows.len() || ids.iter().any(|id| self.retired_ids.contains(id)) {
            return Err(ComponentHostError::Invalid(
                "duplicate or retired id".to_owned(),
            ));
        }
        let root_count = rows.iter().filter(|row| row.parent == 0).count();
        if root_count != 1 {
            return Err(ComponentHostError::Invalid("root count".to_owned()));
        }
        for row in &rows {
            if row.id == 0
                || row.depth > MAX_TREE_DEPTH
                || row.states & !STATE_MASK != 0
                || row.parent == row.id
                || (row.parent != 0 && !ids.contains(&row.parent))
                || row.role > ROLE_NONE
                || row.surface > SURFACE_OVERLAY
            {
                return Err(ComponentHostError::Invalid(
                    "tree identity or depth".to_owned(),
                ));
            }
            if let Some(parent) = rows.iter().find(|candidate| candidate.id == row.parent) {
                if row.depth != parent.depth.saturating_add(1) {
                    return Err(ComponentHostError::Invalid(
                        "tree depth relation".to_owned(),
                    ));
                }
            } else if row.depth != 0 {
                return Err(ComponentHostError::Invalid("root depth".to_owned()));
            }
            if (row.surface == SURFACE_NONE) == (row.role == ROLE_NONE) {
                return Err(ComponentHostError::Invalid(
                    "role and surface mismatch".to_owned(),
                ));
            }
            if let Some(bounds) = row.bounds {
                if bounds.unit != BOUNDS_UNIT_CHARS {
                    return Err(ComponentHostError::Invalid("bounds unit".to_owned()));
                }
            }
            if let Some(focus_cell) = row.focus {
                if focus_cell.row >= MAX_GRID_ROWS || focus_cell.col >= MAX_GRID_COLS {
                    return Err(ComponentHostError::Invalid(
                        "focus coordinate bound".to_owned(),
                    ));
                }
            }
            if let Some(span) = row.selection {
                if span.start_col >= span.end_col || span.end_col > MAX_GRID_COLS {
                    return Err(ComponentHostError::Invalid(
                        "selection span bound".to_owned(),
                    ));
                }
            }
        }
        if let Some(selection) = selection {
            for (row, col) in [
                (selection.anchor_row, selection.anchor_col),
                (selection.focus_row, selection.focus_col),
            ] {
                if row >= MAX_GRID_ROWS || col >= MAX_GRID_COLS {
                    return Err(ComponentHostError::Invalid(
                        "selection coordinate bound".to_owned(),
                    ));
                }
            }
        }
        if let Some(id) = focus {
            if !ids.contains(&id) {
                return Err(ComponentHostError::Invalid("unknown focus id".to_owned()));
            }
            if rows
                .iter()
                .filter(|row| row.states & STATE_FOCUSED != 0)
                .count()
                != 1
                || rows
                    .iter()
                    .all(|row| row.id != id || row.states & STATE_FOCUSED == 0)
            {
                return Err(ComponentHostError::Invalid(
                    "focus state mismatch".to_owned(),
                ));
            }
        } else if rows.iter().any(|row| row.states & STATE_FOCUSED != 0) {
            return Err(ComponentHostError::Invalid(
                "orphan focused state".to_owned(),
            ));
        }
        if selection.is_some() && self.capabilities & CAPABILITY_SELECTION == 0 {
            return Err(ComponentHostError::Peer(ErrorKind::Denied));
        }
        Ok(ComponentProjection {
            generation,
            rows,
            focus,
            selection,
        })
    }
}

fn map_receive_error(error: io::Error) -> ComponentHostError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        ComponentHostError::Deadline
    } else {
        ComponentHostError::ComponentLost
    }
}

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
// Scene text projection (CTX-1628)
// ---------------------------------------------------------------------------

/// Typed refusal from the scene text projection.
///
/// The projection is all-or-nothing: a refused contribution adds neither the
/// scene node nor any text row, so a partially projected scene can never
/// diverge from what the frame paints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneTextProjectionError {
    /// More painted lines than [`MAX_SCENE_TEXT_LINES`].
    LinesTooMany {
        /// Lines supplied.
        lines: usize,
        /// Cap.
        cap: usize,
    },
    /// More painted text bytes than [`MAX_SCENE_TEXT_BYTES`].
    TextTooLarge {
        /// Bytes supplied.
        bytes: usize,
        /// Cap.
        cap: usize,
    },
    /// The projection would exceed the extension's snapshot node ceiling.
    NodesTooMany {
        /// Nodes the projection would produce.
        nodes: usize,
        /// Cap.
        cap: usize,
    },
}

impl fmt::Display for SceneTextProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LinesTooMany { lines, cap } => {
                write!(f, "scene text lines too many: {lines} > {cap}")
            }
            Self::TextTooLarge { bytes, cap } => {
                write!(f, "scene text too large: {bytes} > {cap}")
            }
            Self::NodesTooMany { nodes, cap } => {
                write!(f, "scene projection nodes too many: {nodes} > {cap}")
            }
        }
    }
}

impl std::error::Error for SceneTextProjectionError {}

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
///
/// Notices are a one-shot handoff: [`UiSnapshotHost::push_notice`] queues a
/// transition, and [`SnapshotHost::announcements`] reports the pending notices
/// once and clears them, so the same host value reused across ingests reports
/// each transition exactly once (#2352). The queue is bounded to
/// [`MAX_PENDING_ANNOUNCEMENTS`] with drop-oldest overflow (W-134), and each
/// stored notice is bounded to [`MAX_ANNOUNCEMENT_TEXT_LEN`] (over-limit input
/// is replaced by a fixed marker so the ingest still fails closed). A read
/// consumes the notices even when the surrounding ingest then fails closed
/// (stale generation, over-bound announcement text): those transitions belong
/// to a superseded or invalid generation and are never re-delivered.
#[derive(Clone, Debug)]
pub struct UiSnapshotHost {
    root_name: String,
    title: String,
    rows: Vec<(u32, String)>,
    /// Painted scene text lines (CTX-1628), kept separate from terminal rows
    /// so the projection can prove parity with the paint list and merge into
    /// the snapshot's text runs only at build time.
    scene_text: Vec<(u32, String)>,
    cursor_row: u32,
    cursor_col: u32,
    cursor_visible: bool,
    chrome: Vec<(ChromeKind, String, Option<String>)>,
    scene: Vec<(SceneKind, u64)>,
    interactive: Vec<(InteractivePurpose, String, bool)>,
    modal_overlay: bool,
    focus_label: Option<String>,
    notices: RefCell<VecDeque<(AnnouncementKind, String)>>,
}

impl UiSnapshotHost {
    /// Starts a host naming the window root and the terminal region.
    pub fn new(root_name: &str, title: &str) -> Self {
        Self {
            root_name: root_name.to_string(),
            title: title.to_string(),
            rows: Vec::new(),
            scene_text: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            chrome: Vec::new(),
            scene: Vec::new(),
            interactive: Vec::new(),
            modal_overlay: false,
            focus_label: None,
            notices: RefCell::new(VecDeque::new()),
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

    /// Adds a text-bearing scene node: its kind/identity plus the exact text
    /// runs the frame paints (CTX-1628).
    ///
    /// The pinned accessibility model exposes a scene node as kind + role
    /// only, so scene text contributes through the same bounded text-run
    /// channel as terminal rows. `lines` must be the paint list's own lines
    /// (row ordinal plus text), in paint order: the accessibility tree then
    /// reads exactly what the frame shows, and a caller cannot substitute a
    /// different projection.
    ///
    /// Bounds are the existing ceilings ([`MAX_SCENE_TEXT_LINES`] =
    /// [`MAX_SNAPSHOT_NODES`], and `MAX_SCENE_TEXT_BYTES` =
    /// `MAX_COMPONENT_PROJECTION_BYTES`). Overflow is refused before any
    /// mutation with a typed error: neither the scene node nor a text row is
    /// added, and the host keeps its previous content.
    ///
    /// # Errors
    ///
    /// [`SceneTextProjectionError`] for an over-bound contribution.
    pub fn push_scene_text(
        &mut self,
        kind: SceneKind,
        producer_identity: u64,
        lines: &[(u32, &str)],
    ) -> Result<(), SceneTextProjectionError> {
        if lines.len() > MAX_SCENE_TEXT_LINES {
            return Err(SceneTextProjectionError::LinesTooMany {
                lines: lines.len(),
                cap: MAX_SCENE_TEXT_LINES,
            });
        }
        let bytes = lines
            .iter()
            .map(|(_, text)| text.len())
            .fold(0usize, usize::saturating_add);
        if bytes > MAX_SCENE_TEXT_BYTES {
            return Err(SceneTextProjectionError::TextTooLarge {
                bytes,
                cap: MAX_SCENE_TEXT_BYTES,
            });
        }
        // The extension's node budget: root + terminal leaf + runs + chrome +
        // scene + interactive. Refuse before mutating so the host never holds
        // a projection the adapter would reject at ingest.
        let projected = 2usize
            .saturating_add(self.rows.len())
            .saturating_add(self.scene_text.len())
            .saturating_add(lines.len())
            .saturating_add(self.chrome.len())
            .saturating_add(self.scene.len())
            .saturating_add(1)
            .saturating_add(self.interactive.len());
        if projected > MAX_SNAPSHOT_NODES {
            return Err(SceneTextProjectionError::NodesTooMany {
                nodes: projected,
                cap: MAX_SNAPSHOT_NODES,
            });
        }
        self.scene.push((kind, producer_identity));
        for (row, text) in lines {
            self.scene_text.push((*row, (*text).to_string()));
        }
        Ok(())
    }

    /// Painted scene text lines currently projected, in paint order.
    ///
    /// Read-only parity evidence: this is exactly the text the frame paints
    /// for the scene (same lines, same order), so a caller can assert the
    /// accessibility projection never diverges from the paint list.
    #[must_use]
    pub fn scene_text(&self) -> &[(u32, String)] {
        &self.scene_text
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
    /// [`A11yError::UnknownHandle`], never a guessed owner. A label longer
    /// than [`MAX_ACCESSIBLE_NAME_LEN`] is discarded (nothing is focused)
    /// because it can never match a bounded node.
    pub fn set_focus_label(&mut self, label: Option<&str>) {
        self.focus_label = label
            .filter(|label| label.chars().count() <= MAX_ACCESSIBLE_NAME_LEN)
            .map(str::to_string);
    }

    /// Queues one notice reported by the next ingest.
    ///
    /// The notice is pending until [`SnapshotHost::announcements`] reads it:
    /// that read reports every queued notice once and clears the queue, so a
    /// host value reused across ingests never re-delivers a transition from a
    /// previous ingest (#2352). Pushing past
    /// [`MAX_PENDING_ANNOUNCEMENTS`] drops the oldest notice, keeping the
    /// documented W-134 drop-oldest bound; the ingest does not fail.
    ///
    /// Callers that abandon a generation's transitions (for example after
    /// replacing the host content wholesale) can discard the pending notices
    /// with [`Self::clear_notices`]. Text above the 256-character announcement
    /// ceiling is never retained and never silently truncated or dropped: the
    /// host stores a fixed over-limit marker instead, so the adapter still
    /// rejects the ingest fail-closed with
    /// [`A11yError::AnnouncementTooLong`] and the original payload is not kept
    /// in memory.
    pub fn push_notice(&mut self, kind: AnnouncementKind, text: &str) {
        let stored = if text.chars().count() <= MAX_ANNOUNCEMENT_TEXT_LEN {
            text.to_string()
        } else {
            "!".repeat(MAX_ANNOUNCEMENT_TEXT_LEN + 1)
        };
        let notices = self.notices.get_mut();
        if notices.len() >= MAX_PENDING_ANNOUNCEMENTS {
            notices.pop_front();
        }
        notices.push_back((kind, stored));
    }

    /// Discards every notice queued for the next ingest.
    ///
    /// Notices otherwise clear themselves when [`SnapshotHost::announcements`]
    /// reads them; this is the explicit hook for transitions that no longer
    /// belong to the generation about to be reported.
    pub fn clear_notices(&mut self) {
        self.notices.get_mut().clear();
    }
}

impl SnapshotHost for UiSnapshotHost {
    fn build_snapshot(&self, builder: &mut SnapshotBuilder) -> Result<(), A11yError> {
        builder.set_root_name(&self.root_name)?;
        let runs: Vec<TextRun> = self
            .rows
            .iter()
            .chain(self.scene_text.iter())
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
        // One-shot handoff: the notices queued for this ingest are reported
        // once and cleared, so a host value reused across ingests reports
        // only the transitions pushed since the previous read (#2352).
        self.notices.borrow_mut().drain(..).collect()
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

    use std::collections::VecDeque;

    use bitty_a11y::Generation;
    use bitty_a11y_component::component::protocol::{ROLE_GROUP, ROLE_TEXT};

    #[derive(Debug, Default)]
    struct MockTransport {
        incoming: VecDeque<Message>,
        sent: Vec<Message>,
        receive_error: Option<io::ErrorKind>,
    }

    impl MockTransport {
        fn with_messages(messages: impl IntoIterator<Item = Message>) -> Self {
            Self {
                incoming: messages.into_iter().collect(),
                sent: Vec::new(),
                receive_error: None,
            }
        }
    }

    impl ComponentTransport for MockTransport {
        fn send(&mut self, message: &Message) -> io::Result<()> {
            self.sent.push(message.clone());
            Ok(())
        }

        fn receive(&mut self, _deadline: Instant) -> io::Result<Option<Message>> {
            if let Some(kind) = self.receive_error.take() {
                return Err(io::Error::from(kind));
            }
            Ok(self.incoming.pop_front())
        }
    }

    fn component_config() -> ComponentHostConfig {
        ComponentHostConfig {
            safe_mode: false,
            capability_granted: true,
            deadline: Duration::from_secs(1),
        }
    }

    fn row(id: u64, parent: u64, depth: u8, role: u8, states: u16) -> TreeRow {
        TreeRow {
            id,
            parent,
            depth,
            role,
            surface: SURFACE_NONE,
            name: Some(format!("node-{id}")),
            content: None,
            state_text: None,
            states,
            focus: None,
            selection: None,
            bounds: None,
        }
    }

    fn snapshot_messages(generation: u64, rows: Vec<TreeRow>) -> Vec<Message> {
        vec![
            Message::HelloAck {
                protocol: PROTOCOL_VERSION,
                component: "a11y".to_owned(),
                version: "0.0.1".to_owned(),
                capabilities: CAPABILITY_SNAPSHOT | CAPABILITY_DIFF | CAPABILITY_SELECTION,
            },
            Message::SnapshotBegin {
                request: 1,
                generation,
                nodes: rows.len() as u32,
                focus: None,
                selection: None,
                pages: 1,
            },
            Message::SnapshotPage {
                request: 1,
                index: 0,
                rows,
                last: true,
            },
        ]
    }

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

    #[test]
    fn scene_text_projection_carries_kind_identity_and_runs() {
        let mut host = UiSnapshotHost::new("window", "shell");
        host.push_scene_text(
            SceneKind::Table,
            7,
            &[(0, "name | value"), (1, "alpha | 1")],
        )
        .expect("bounded");
        let generation = Generation::initial();
        let mut adapter = bitty_a11y::Adapter::new();
        adapter
            .ingest_host(&host, generation, None)
            .expect("host ingest");
        let snapshot = adapter.snapshot().expect("snapshot");
        let mut scenes = 0usize;
        let mut rows = Vec::new();
        for handle in snapshot.preorder() {
            match snapshot.get(handle) {
                Some(bitty_a11y::NodeView::Scene { kind, .. }) => {
                    scenes += 1;
                    assert_eq!(kind, SceneKind::Table);
                }
                Some(bitty_a11y::NodeView::TerminalRow { text }) => rows.push((*text).to_string()),
                _ => {}
            }
        }
        assert_eq!(scenes, 1, "the scene kind/identity is projected");
        assert_eq!(rows, vec!["name | value", "alpha | 1"]);
    }

    #[test]
    fn scene_text_projection_refuses_over_bound_contributions_atomically() {
        let mut host = UiSnapshotHost::new("window", "shell");
        let too_many = vec![(0u32, "x"); MAX_SCENE_TEXT_LINES + 1];
        assert!(matches!(
            host.push_scene_text(SceneKind::Text, 1, &too_many),
            Err(SceneTextProjectionError::LinesTooMany { .. })
        ));
        assert!(host.scene.is_empty(), "no scene node on refusal");
        assert!(host.scene_text.is_empty(), "no text row on refusal");

        let big = "x".repeat(MAX_SCENE_TEXT_BYTES + 1);
        assert!(matches!(
            host.push_scene_text(SceneKind::Text, 1, &[(0, big.as_str())]),
            Err(SceneTextProjectionError::TextTooLarge { .. })
        ));
        assert!(host.scene.is_empty());
        assert!(host.scene_text.is_empty());
    }

    #[test]
    fn scene_text_projection_respects_the_snapshot_node_ceiling() {
        let mut host = UiSnapshotHost::new("window", "shell");
        for row in 0..(MAX_SNAPSHOT_NODES - 2) {
            host.push_row(row as u32, "x");
        }
        // Root + terminal leaf + rows + one scene node + one run already
        // exceeds the ceiling.
        assert!(matches!(
            host.push_scene_text(SceneKind::Text, 1, &[(0, "one")]),
            Err(SceneTextProjectionError::NodesTooMany { .. })
        ));
        assert!(host.scene.is_empty());
        assert_eq!(host.rows.len(), MAX_SNAPSHOT_NODES - 2);

        // One row fewer leaves exactly the two nodes a single-line scene
        // needs.
        let mut host = UiSnapshotHost::new("window", "shell");
        for row in 0..(MAX_SNAPSHOT_NODES - 4) {
            host.push_row(row as u32, "x");
        }
        host.push_scene_text(SceneKind::Text, 2, &[(0, "one")])
            .expect("last node fits");
    }

    #[test]
    fn notices_drain_once_and_stay_bounded() {
        let mut host = UiSnapshotHost::new("window", "shell");
        for index in 0..(MAX_PENDING_ANNOUNCEMENTS + 2) {
            host.push_notice(AnnouncementKind::AsyncState, &format!("state {index}"));
        }
        let notices = host.announcements();
        assert_eq!(notices.len(), MAX_PENDING_ANNOUNCEMENTS);
        assert_eq!(notices.first().expect("nonempty").1, "state 2");
        assert_eq!(
            notices.last().expect("nonempty").1,
            format!("state {}", MAX_PENDING_ANNOUNCEMENTS + 1)
        );
        assert!(
            host.announcements().is_empty(),
            "reading the notices must clear them"
        );
    }

    #[test]
    fn clear_notices_discards_pending_transitions() {
        let mut host = UiSnapshotHost::new("window", "shell");
        host.push_notice(AnnouncementKind::Focus, "shell focused");
        host.clear_notices();
        assert!(host.announcements().is_empty());
    }

    #[test]
    fn overlong_notice_text_is_not_retained() {
        let mut host = UiSnapshotHost::new("window", "shell");
        let payload = "x".repeat(MAX_ANNOUNCEMENT_TEXT_LEN * 8);
        host.push_notice(AnnouncementKind::AsyncState, &payload);
        let notices = host.announcements();
        assert_eq!(notices.len(), 1);
        let stored = &notices[0].1;
        assert_eq!(stored.chars().count(), MAX_ANNOUNCEMENT_TEXT_LEN + 1);
        assert_ne!(stored, &payload, "the original payload must not be kept");
    }

    #[test]
    fn component_host_denies_safe_mode_before_transport() {
        let transport = MockTransport::default();
        let result = ComponentSnapshotHost::connect(transport, ComponentHostConfig::default());
        assert!(matches!(result, Err(ComponentHostError::Denied)));
    }

    #[test]
    fn component_host_accepts_bounded_snapshot_and_handshakes() {
        let rows = vec![row(1, 0, 0, ROLE_GROUP, 0), row(2, 1, 1, ROLE_TEXT, 0)];
        let transport = MockTransport::with_messages(snapshot_messages(1, rows.clone()));
        let mut host = ComponentSnapshotHost::connect(transport, component_config())
            .expect("component handshake");
        let projection = host.refresh().expect("bounded snapshot");
        assert_eq!(projection.generation(), 1);
        assert_eq!(projection.rows(), rows);
        assert_eq!(
            host.projection().expect("retained projection").rows().len(),
            2
        );
    }

    #[test]
    fn component_host_rejects_stale_generation_and_clears_projection() {
        let rows = vec![row(1, 0, 0, ROLE_GROUP, 0)];
        let mut messages = snapshot_messages(1, rows);
        messages.extend([Message::SnapshotBegin {
            request: 2,
            generation: 1,
            nodes: 1,
            focus: None,
            selection: None,
            pages: 1,
        }]);
        let transport = MockTransport::with_messages(messages);
        let mut host = ComponentSnapshotHost::connect(transport, component_config())
            .expect("component handshake");
        host.refresh().expect("initial snapshot");
        assert!(matches!(
            host.refresh(),
            Err(ComponentHostError::Invalid(_))
        ));
        assert!(host.projection().is_none(), "stale data must fail closed");
    }

    #[test]
    fn component_host_rejects_unknown_diff_removal() {
        let rows = vec![row(1, 0, 0, ROLE_GROUP, 0)];
        let mut messages = snapshot_messages(1, rows);
        messages.extend([
            Message::DiffBegin {
                request: 2,
                base: 1,
                generation: 2,
                removed: 1,
                upserts: 0,
                focus: None,
                selection: None,
                pages: 1,
            },
            Message::DiffPage {
                request: 2,
                index: 0,
                removed: vec![99],
                upserts: Vec::new(),
                last: true,
            },
        ]);
        let transport = MockTransport::with_messages(messages);
        let mut host = ComponentSnapshotHost::connect(transport, component_config())
            .expect("component handshake");
        host.refresh().expect("initial snapshot");
        assert!(matches!(
            host.refresh(),
            Err(ComponentHostError::Invalid(_))
        ));
        assert!(host.projection().is_none(), "unknown ids must fail closed");
    }

    #[test]
    fn component_host_accepts_same_generation_empty_diff() {
        let transport = MockTransport::with_messages([
            Message::HelloAck {
                protocol: PROTOCOL_VERSION,
                component: "a11y".to_owned(),
                version: "0.0.1".to_owned(),
                capabilities: CAPABILITY_SNAPSHOT | CAPABILITY_DIFF,
            },
            Message::SnapshotBegin {
                request: 1,
                generation: 1,
                nodes: 1,
                focus: None,
                selection: None,
                pages: 1,
            },
            Message::SnapshotPage {
                request: 1,
                index: 0,
                rows: vec![row(1, 0, 0, ROLE_GROUP, 0)],
                last: true,
            },
            Message::DiffBegin {
                request: 2,
                base: 1,
                generation: 1,
                removed: 0,
                upserts: 0,
                focus: None,
                selection: None,
                pages: 0,
            },
        ]);
        let mut host = ComponentSnapshotHost::connect(transport, component_config())
            .expect("component handshake");
        host.refresh().expect("initial snapshot");
        assert_eq!(host.refresh().expect("empty diff").generation(), 1);
    }

    #[test]
    fn component_eof_clears_last_projection() {
        let transport =
            MockTransport::with_messages(snapshot_messages(1, vec![row(1, 0, 0, ROLE_GROUP, 0)]));
        let mut host = ComponentSnapshotHost::connect(transport, component_config())
            .expect("component handshake");
        host.refresh().expect("initial snapshot");
        assert_eq!(host.refresh(), Err(ComponentHostError::ComponentLost));
        assert!(host.projection().is_none(), "EOF must fail closed");
    }

    #[test]
    fn component_timeout_clears_last_projection() {
        let mut transport =
            MockTransport::with_messages(snapshot_messages(1, vec![row(1, 0, 0, ROLE_GROUP, 0)]));
        transport.receive_error = Some(io::ErrorKind::TimedOut);
        let result = ComponentSnapshotHost::connect(transport, component_config());
        assert!(matches!(result, Err(ComponentHostError::Deadline)));
    }
}
