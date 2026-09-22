//! Declarative panel rehydration (UX-27, CTX-0672).
//!
//! Candidate implementation of U-7
//! (`bitty-terminal-docs/specifications/ui-runtime-candidate.md`)
//! (**Candidate**, owner-pending Panel RFC). Nothing here is normative,
//! accepted, or verified: every spelling, bound, and rule below is a
//! candidate that the Panel RFC accepts or rejects, never this module.
//! The module is English-only.
//!
//! What this module provides:
//!
//! - [`PanelRecord`] — the declarative snapshot of one panel's seven
//!   axes ([`SevenPanelState`](crate::panel_state::SevenPanelState)) as
//!   plain values, with a `k=v;...` text encoding that uses no serializer
//!   and no heap image.
//! - [`encode_snapshot`] / [`rehydrate_snapshot`] — a snapshot document
//!   is newline-joined records; rehydration builds **fresh** states and
//!   re-applies values only. A VM heap is never deserialized: there is no
//!   `unsafe`, no pointer, and no allocator image anywhere in this path.
//! - Failure and version policy (candidate, ordering/failure/migration
//!   stay open in the RFC, this spelling is only the headless default):
//!   unknown `k=` fields are ignored (forward tolerance), missing fields
//!   take parked defaults, a record pinned to an unsupported `v=` rejects
//!   the whole document ([`RehydrateError::UnsupportedVersion`]), and a
//!   malformed record rejects the whole document (fails closed, applies
//!   nothing). Budget caps ([`MAX_SNAPSHOT_PANELS`], [`MAX_RECORD_LEN`])
//!   reject before any state is built.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.
//! Rehydration output is sorted by [`PanelId`](crate::panel::PanelId), so
//! reports are deterministic.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;

use crate::panel::PanelId;
use crate::panel_state::{
    MAX_BADGE_TEXT_LEN, PanelActivity, PanelAttention, PanelFocusState, PanelInteraction,
    PanelLifecycle, PanelStateError, PanelVisibility, SevenPanelState,
};
use crate::presentation::PresentationMode;
use crate::uitree::UiNodeId;

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

/// Snapshot format version written by [`encode_snapshot`].
pub const SNAPSHOT_VERSION: u32 = 1;

/// Hard cap on records per snapshot document.
///
/// Rejected with [`RehydrateError::TooManyPanels`], never silently pruned.
pub const MAX_SNAPSHOT_PANELS: usize = 256;

/// Hard cap in characters on one encoded record line.
///
/// Rejected with [`RehydrateError::RecordTooLong`], never silently cut.
pub const MAX_RECORD_LEN: usize = 2048;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to encode or rehydrate a panel snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RehydrateError {
    /// A record pins an unsupported format version.
    UnsupportedVersion {
        /// Version found on the record.
        found: u32,
        /// Version this build writes.
        supported: u32,
    },
    /// A record line is malformed (fails closed, applies nothing).
    Malformed {
        /// 1-based line number in the submitted document.
        line: usize,
    },
    /// A record carries an unknown axis value (known key, bad value).
    BadValue {
        /// 1-based line number in the submitted document.
        line: usize,
        /// The offending key.
        key: String,
    },
    /// The document holds more than [`MAX_SNAPSHOT_PANELS`] records.
    TooManyPanels {
        /// Records counted in the submitted document.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A record line exceeds [`MAX_RECORD_LEN`] characters.
    RecordTooLong {
        /// 1-based line number in the submitted document.
        line: usize,
        /// Characters counted on the line.
        found: usize,
    },
    /// Two records claim the same panel.
    DuplicatePanel {
        /// The repeated raw panel id.
        panel: u64,
    },
    /// Axis state failed to build (for example badge text over budget).
    State(PanelStateError),
}

impl fmt::Display for RehydrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } => {
                write!(
                    f,
                    "unsupported snapshot version {found}, supported {supported}"
                )
            }
            Self::Malformed { line } => write!(f, "malformed record at line {line}"),
            Self::BadValue { line, key } => {
                write!(f, "bad value for '{key}' at line {line}")
            }
            Self::TooManyPanels { found, cap } => {
                write!(f, "too many snapshot panels: {found}, cap {cap}")
            }
            Self::RecordTooLong { line, found } => {
                write!(f, "record too long at line {line}: {found} chars")
            }
            Self::DuplicatePanel { panel } => {
                write!(f, "duplicate snapshot panel {panel}")
            }
            Self::State(inner) => write!(f, "panel state error: {inner}"),
        }
    }
}

impl std::error::Error for RehydrateError {}

impl From<PanelStateError> for RehydrateError {
    fn from(inner: PanelStateError) -> Self {
        Self::State(inner)
    }
}

// ---------------------------------------------------------------------------
// Record: declarative capture of the seven axes
// ---------------------------------------------------------------------------

/// Declarative snapshot of one panel's seven axes as plain values.
///
/// Captured with [`PanelRecord::capture`] from a live
/// [`SevenPanelState`] and re-applied with [`PanelRecord::apply_fresh`],
/// which builds a brand-new state: no heap image crosses the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRecord {
    /// Raw panel identity.
    pub panel: u64,
    /// Raw tree-node identity.
    pub node: u64,
    /// Axis 1: lifecycle.
    pub lifecycle: PanelLifecycle,
    /// Axis 2: presentation (accepted mode type).
    pub presentation: PresentationMode,
    /// Axis 3: visibility.
    pub visibility: PanelVisibility,
    /// Axis 4: focus.
    pub focus: PanelFocusState,
    /// Axis 5: attention (badges included).
    pub attention: PanelAttention,
    /// Axis 6: interaction.
    pub interaction: PanelInteraction,
    /// Axis 7: activity.
    pub activity: PanelActivity,
}

impl PanelRecord {
    /// Captures the seven axes of a live state as plain values.
    #[must_use]
    pub fn capture(state: &SevenPanelState) -> Self {
        Self {
            panel: state.panel().get(),
            node: state.node().get(),
            lifecycle: state.lifecycle(),
            presentation: state.presentation(),
            visibility: state.visibility(),
            focus: state.focus(),
            attention: state.attention().clone(),
            interaction: state.interaction(),
            activity: state.activity(),
        }
    }

    /// Builds a fresh state with the recorded values re-applied.
    ///
    /// Fresh-VM rule: the returned state is newly constructed; nothing is
    /// deserialized from a heap image, only axis values are copied over.
    /// The lifecycle axis replays through the transition gate, and a
    /// recorded focus re-owns routing through `focus_panel`, so a focused
    /// record ends focused on the fresh VM.
    pub fn apply_fresh(&self) -> Result<SevenPanelState, RehydrateError> {
        let mut fresh = SevenPanelState::new(PanelId::new(self.panel), UiNodeId::new(self.node));
        fresh.set_presentation(self.presentation);
        fresh.set_visibility(self.visibility);
        fresh.set_attention(self.attention.clone());
        fresh.set_interaction(self.interaction);
        fresh.set_activity(self.activity);
        replay_lifecycle(&mut fresh, self.lifecycle)?;
        if self.focus == PanelFocusState::Focused && fresh.lifecycle() == PanelLifecycle::Mounted {
            fresh.focus_panel().map_err(RehydrateError::from)?;
        }
        Ok(fresh)
    }

    /// Encodes the record as one `k=v;...` line (keys in fixed order).
    #[must_use]
    pub fn encode(&self) -> String {
        let (attention_kind, attention_count, attention_text) = match &self.attention {
            PanelAttention::Quiet => ("quiet", 0, String::new()),
            PanelAttention::Badge { count, text } => ("badge", *count, escape(text)),
            PanelAttention::Highlight => ("highlight", 0, String::new()),
            PanelAttention::Urgent => ("urgent", 0, String::new()),
        };
        format!(
            "v={};panel={};node={};lifecycle={};presentation={};visibility={};focus={};attention={};attention_count={};attention_text={};interaction={};activity={}",
            SNAPSHOT_VERSION,
            self.panel,
            self.node,
            self.lifecycle.as_str(),
            presentation_str(self.presentation),
            self.visibility.as_str(),
            self.focus.as_str(),
            attention_kind,
            attention_count,
            attention_text,
            self.interaction.as_str(),
            self.activity.as_str(),
        )
    }

    /// Decodes one record line. Unknown `k=` fields are ignored (forward
    /// tolerance); missing fields take parked defaults; a missing `panel`
    /// key or a bad escape rejects the line.
    pub fn decode(line: &str) -> Result<Self, RehydrateError> {
        decode_record(line, 1)
    }
}

/// Replays a recorded lifecycle onto a fresh `Declared` state through the
/// transition gate. `Focused` replays as `Mounted` (routing is re-owned
/// by [`apply_fresh`](PanelRecord::apply_fresh) via the focus axis).
fn replay_lifecycle(
    fresh: &mut SevenPanelState,
    lifecycle: PanelLifecycle,
) -> Result<(), RehydrateError> {
    match lifecycle {
        PanelLifecycle::Declared => Ok(()),
        PanelLifecycle::Created => fresh
            .set_lifecycle(PanelLifecycle::Created)
            .map_err(RehydrateError::from),
        PanelLifecycle::Mounted | PanelLifecycle::Focused => {
            fresh
                .set_lifecycle(PanelLifecycle::Created)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Mounted)
                .map_err(RehydrateError::from)?;
            Ok(())
        }
        PanelLifecycle::Suspended => {
            fresh
                .set_lifecycle(PanelLifecycle::Created)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Mounted)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Suspended)
                .map_err(RehydrateError::from)?;
            Ok(())
        }
        PanelLifecycle::Closed => {
            fresh
                .set_lifecycle(PanelLifecycle::Created)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Mounted)
                .map_err(RehydrateError::from)?;
            fresh.close().map_err(RehydrateError::from)?;
            Ok(())
        }
        PanelLifecycle::Disposed => {
            fresh
                .set_lifecycle(PanelLifecycle::Created)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Mounted)
                .map_err(RehydrateError::from)?;
            fresh
                .set_lifecycle(PanelLifecycle::Disposed)
                .map_err(RehydrateError::from)?;
            Ok(())
        }
    }
}

/// Canonical snapshot spelling of the accepted presentation mode.
fn presentation_str(mode: PresentationMode) -> &'static str {
    match mode {
        PresentationMode::Tiled => "tiled",
        PresentationMode::Floating => "floating",
        PresentationMode::Fullscreen => "fullscreen",
        PresentationMode::Scratchpad => "scratchpad",
    }
}

/// Parses the canonical snapshot spelling of the presentation mode.
fn parse_presentation(s: &str) -> Option<PresentationMode> {
    match s {
        "tiled" => Some(PresentationMode::Tiled),
        "floating" => Some(PresentationMode::Floating),
        "fullscreen" => Some(PresentationMode::Fullscreen),
        "scratchpad" => Some(PresentationMode::Scratchpad),
        _ => None,
    }
}

/// Escapes badge text for a `;`-joined record (`\` + `n`, `;`, `=`, `\`).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            ';' => out.push_str("\\;"),
            '=' => out.push_str("\\="),
            _ => out.push(ch),
        }
    }
    out
}

/// Unescapes badge text; a trailing lone `\` fails closed.
fn unescape(text: &str, line: usize) -> Result<String, RehydrateError> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let Some(next) = chars.next() else {
            return Err(RehydrateError::Malformed { line });
        };
        match next {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            ';' => out.push(';'),
            '=' => out.push('='),
            _ => return Err(RehydrateError::Malformed { line }),
        }
    }
    Ok(out)
}

/// Splits a record line on unescaped `;` (a `\;` escape stays inside
/// its field for [`unescape`] to handle later).
fn split_fields(line: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            cur.push(ch);
            if let Some(next) = chars.next() {
                cur.push(next);
            }
            continue;
        }
        if ch == ';' {
            fields.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(ch);
    }
    fields.push(cur);
    fields
}

fn bad(line: usize, key: &str) -> RehydrateError {
    RehydrateError::BadValue {
        line,
        key: key.to_owned(),
    }
}

fn parse_u64(value: &str, line: usize, key: &str) -> Result<u64, RehydrateError> {
    value.parse::<u64>().map_err(|_| bad(line, key))
}

fn parse_u32(value: &str, line: usize, key: &str) -> Result<u32, RehydrateError> {
    value.parse::<u32>().map_err(|_| bad(line, key))
}

/// Decodes one record line with its 1-based document line number.
fn decode_record(line: &str, line_no: usize) -> Result<PanelRecord, RehydrateError> {
    if line.chars().count() > MAX_RECORD_LEN {
        return Err(RehydrateError::RecordTooLong {
            line: line_no,
            found: line.chars().count(),
        });
    }
    let mut panel: Option<u64> = None;
    let mut node: u64 = 0;
    let mut version: u32 = SNAPSHOT_VERSION;
    let mut lifecycle = PanelLifecycle::Declared;
    let mut presentation = PresentationMode::Tiled;
    let mut visibility = PanelVisibility::Hidden;
    let mut focus = PanelFocusState::Unfocused;
    let mut attention_kind = String::from("quiet");
    let mut attention_count: u32 = 0;
    let mut attention_text = String::new();
    let mut interaction = PanelInteraction::Idle;
    let mut activity = PanelActivity::Suspended;

    for field in split_fields(line) {
        if field.is_empty() {
            continue;
        }
        let Some((key, value)) = field.split_once('=') else {
            return Err(RehydrateError::Malformed { line: line_no });
        };
        match key {
            "v" => {
                version = parse_u32(value, line_no, key)?;
            }
            "panel" => {
                panel = Some(parse_u64(value, line_no, key)?);
            }
            "node" => {
                node = parse_u64(value, line_no, key)?;
            }
            "lifecycle" => {
                let Some(parsed) = PanelLifecycle::parse(value) else {
                    return Err(bad(line_no, key));
                };
                lifecycle = parsed;
            }
            "presentation" => {
                let Some(parsed) = parse_presentation(value) else {
                    return Err(bad(line_no, key));
                };
                presentation = parsed;
            }
            "visibility" => {
                let Some(parsed) = PanelVisibility::parse(value) else {
                    return Err(bad(line_no, key));
                };
                visibility = parsed;
            }
            "focus" => {
                let Some(parsed) = PanelFocusState::parse(value) else {
                    return Err(bad(line_no, key));
                };
                focus = parsed;
            }
            "attention" => {
                attention_kind = match value {
                    "quiet" | "badge" | "highlight" | "urgent" => value.to_owned(),
                    _ => return Err(bad(line_no, key)),
                };
            }
            "attention_count" => {
                attention_count = parse_u32(value, line_no, key)?;
            }
            "attention_text" => {
                attention_text = unescape(value, line_no)?;
            }
            "interaction" => {
                let Some(parsed) = PanelInteraction::parse(value) else {
                    return Err(bad(line_no, key));
                };
                interaction = parsed;
            }
            "activity" => {
                let Some(parsed) = PanelActivity::parse(value) else {
                    return Err(bad(line_no, key));
                };
                activity = parsed;
            }
            // Unknown fields are ignored: newer writers stay readable.
            _ => {}
        }
    }

    if version != SNAPSHOT_VERSION {
        return Err(RehydrateError::UnsupportedVersion {
            found: version,
            supported: SNAPSHOT_VERSION,
        });
    }
    let Some(panel) = panel else {
        return Err(RehydrateError::Malformed { line: line_no });
    };

    let attention = match attention_kind.as_str() {
        "badge" => {
            if attention_text.chars().count() > MAX_BADGE_TEXT_LEN {
                return Err(RehydrateError::BadValue {
                    line: line_no,
                    key: "attention_text".to_owned(),
                });
            }
            PanelAttention::Badge {
                count: attention_count,
                text: attention_text,
            }
        }
        "highlight" => PanelAttention::Highlight,
        "urgent" => PanelAttention::Urgent,
        _ => PanelAttention::Quiet,
    };

    Ok(PanelRecord {
        panel,
        node,
        lifecycle,
        presentation,
        visibility,
        focus,
        attention,
        interaction,
        activity,
    })
}

// ---------------------------------------------------------------------------
// Snapshot document: encode + declarative rehydration
// ---------------------------------------------------------------------------

/// Outcome of [`rehydrate_snapshot`]: fresh states plus what was applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehydrateReport {
    /// Fresh states, sorted by [`PanelId`](crate::panel::PanelId).
    pub states: Vec<SevenPanelState>,
    /// Raw panel ids applied, in ascending order.
    pub applied: Vec<u64>,
}

impl RehydrateReport {
    /// Looks up a rehydrated state by panel id.
    #[must_use]
    pub fn get(&self, panel: PanelId) -> Option<&SevenPanelState> {
        self.states.iter().find(|s| s.panel() == panel)
    }
}

/// Encodes panel states as a snapshot document (one record per line).
/// Output is sorted by panel id, so identical states encode identically.
#[must_use]
pub fn encode_snapshot(states: &[SevenPanelState]) -> String {
    let mut records: Vec<String> = states
        .iter()
        .map(|s| PanelRecord::capture(s).encode())
        .collect();
    records.sort();
    records.join("\n")
}

/// Rehydrates a snapshot document into fresh states.
///
/// Blank lines and `#` comment lines are skipped. Unknown `k=` fields are
/// ignored. Any malformed record, bad value, duplicate panel, unsupported
/// version, or budget breach rejects the whole document and applies
/// nothing (fails closed).
pub fn rehydrate_snapshot(doc: &str) -> Result<RehydrateReport, RehydrateError> {
    let mut records: Vec<PanelRecord> = Vec::new();
    let mut seen: BTreeMap<u64, ()> = BTreeMap::new();
    for (index, raw_line) in doc.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let record = decode_record(line, line_no)?;
        if seen.contains_key(&record.panel) {
            return Err(RehydrateError::DuplicatePanel {
                panel: record.panel,
            });
        }
        seen.insert(record.panel, ());
        records.push(record);
    }
    if records.len() > MAX_SNAPSHOT_PANELS {
        return Err(RehydrateError::TooManyPanels {
            found: records.len(),
            cap: MAX_SNAPSHOT_PANELS,
        });
    }
    records.sort_by_key(|r| r.panel);
    let mut states: Vec<SevenPanelState> = Vec::with_capacity(records.len());
    for record in &records {
        states.push(record.apply_fresh()?);
    }
    let applied: Vec<u64> = records.iter().map(|r| r.panel).collect();
    Ok(RehydrateReport { states, applied })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted(panel: u64) -> SevenPanelState {
        let mut s = SevenPanelState::new(PanelId::new(panel), UiNodeId::new(panel + 1000));
        s.set_lifecycle(PanelLifecycle::Created).unwrap();
        s.set_lifecycle(PanelLifecycle::Mounted).unwrap();
        s.set_visibility(PanelVisibility::Visible);
        s
    }

    #[test]
    fn record_round_trip_covers_all_axes() {
        let mut s = mounted(7);
        s.set_presentation(PresentationMode::Floating);
        s.set_attention(PanelAttention::badge(42, "msgs;=\\end").unwrap());
        s.set_interaction(PanelInteraction::Typing);
        s.set_activity(PanelActivity::Active);
        let line = PanelRecord::capture(&s).encode();
        let back = PanelRecord::decode(&line).unwrap();
        assert_eq!(back, PanelRecord::capture(&s));
        let fresh = back.apply_fresh().unwrap();
        assert_eq!(fresh.lifecycle(), PanelLifecycle::Mounted);
        assert_eq!(fresh.presentation(), PresentationMode::Floating);
        assert_eq!(fresh.visibility(), PanelVisibility::Visible);
        assert_eq!(fresh.attention(), s.attention());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let line = format!(
            "{};zzz_future=1;another=x",
            PanelRecord::capture(&mounted(3)).encode()
        );
        let back = PanelRecord::decode(&line).unwrap();
        assert_eq!(back.panel, 3);
    }

    #[test]
    fn unsupported_version_rejects() {
        let line = PanelRecord::capture(&mounted(3))
            .encode()
            .replacen("v=1;", "v=2;", 1);
        assert!(matches!(
            PanelRecord::decode(&line),
            Err(RehydrateError::UnsupportedVersion { found: 2, .. })
        ));
    }
}
