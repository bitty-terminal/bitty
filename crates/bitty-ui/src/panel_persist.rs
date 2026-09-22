//! Restart persistence decision support (UX-07, issue #1013).
//!
//! Candidate implementation (**Candidate**, owner-pending Panel RFC;
//! the identity-persistence decision itself is `[BLOCKED: RFC-OQ-9]`).
//! Nothing here is normative, accepted, or verified: this module does not
//! decide whether panel/workspace identity persists across restart. It
//! provides the headless mechanics both outcomes need, so the RFC decides
//! between spellings that already run. Every bound and spelling below is a
//! candidate the owning RFC accepts or rejects, never this module. The
//! module is English-only.
//!
//! What this module provides:
//!
//! - [`PersistencePolicy`] — the two candidate outcomes (`Ephemeral`
//!   mints fresh ids every restart; `StableIdentity` reuses persisted
//!   ids). The default is [`PersistencePolicy::Ephemeral`]: without an
//!   owner decision, restart must not resurrect identities that may have
//!   been retired.
//! - [`PersistedPanel`] / [`RestartManifest`] — the declarative restart
//!   record: which panels existed, with which title and workspace index.
//!   A manifest carries no heap image, no pointer, and no allocator state.
//! - [`encode_manifest`] / [`decode_manifest`] — the newline-joined
//!   `v=<n>;id=<n>;ws=<n>;title=<text>` document spelling with the same
//!   fail-closed policy as panel snapshots: unknown `k=` fields are
//!   ignored (forward tolerance), a record pinned to an unsupported `v=`
//!   rejects the whole document, and a malformed record rejects the whole
//!   document with nothing applied.
//! - [`plan_restore`] — the deterministic restore order under
//!   [`PersistencePolicy::StableIdentity`]: sorted by persisted id, so
//!   identical manifests produce identical re-attachment sequences.
//!
//! Relationship to [`panel_rehydrate`](crate::panel_rehydrate): that
//! module snapshots seven-axis panel *state*; this module records
//! *existence* (which panels and titles belong to which workspace index).
//! State re-application stays with rehydration; identity re-attachment
//! planning lives here.
//!
//! All types are bounded and `#![forbid(unsafe_code)]`. No wall-clock time,
//! randomness, render, platform, PTY, or plugin handle participates.

#![forbid(unsafe_code)]

use std::fmt;

use crate::panel::PanelId;
use crate::panel_identity::MAX_IDENTITY_TITLE_LEN;

/// Manifest format version written by [`encode_manifest`].
pub const PERSIST_MANIFEST_VERSION: u32 = 1;

/// Hard cap on panels per restart manifest.
///
/// Rejected with [`ManifestError::TooManyPanels`], never silently pruned:
/// pruning would restart with a partial workspace presented as complete.
pub const MAX_PERSIST_PANELS: usize = 256;

/// Hard cap in characters on one encoded manifest line.
///
/// Rejected with [`ManifestError::LineTooLong`], never silently cut.
pub const MAX_MANIFEST_LINE_LEN: usize = 512;

// ---------------------------------------------------------------------------
// PersistencePolicy: the open decision, as data
// ---------------------------------------------------------------------------

/// Candidate restart outcomes for panel identity (decision: `RFC-OQ-9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PersistencePolicy {
    /// Fresh ids every restart; the manifest is advisory (titles and
    /// workspace indices) only. Fail-safe default while the RFC is open.
    Ephemeral,
    /// Reuses the persisted ids via [`plan_restore`]. Requires the RFC to
    /// accept stable cross-restart identity first.
    StableIdentity,
}

impl PersistencePolicy {
    /// Canonical lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::StableIdentity => "stable-identity",
        }
    }
}

impl fmt::Display for PersistencePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to encode, decode, or plan a restart manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
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
    /// A record line exceeds [`MAX_MANIFEST_LINE_LEN`] characters.
    LineTooLong {
        /// 1-based line number in the submitted document.
        line: usize,
    },
    /// The document holds more than [`MAX_PERSIST_PANELS`] records.
    TooManyPanels {
        /// Records counted in the submitted document.
        found: usize,
        /// The cap that was exceeded.
        cap: usize,
    },
    /// A title exceeds [`MAX_IDENTITY_TITLE_LEN`] characters.
    TitleTooLong {
        /// 1-based line number in the submitted document.
        line: usize,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found, supported } => {
                write!(
                    f,
                    "unsupported manifest version {found}, supported {supported}"
                )
            }
            Self::Malformed { line } => write!(f, "malformed manifest record at line {line}"),
            Self::LineTooLong { line } => {
                write!(
                    f,
                    "manifest line {line} exceeds {MAX_MANIFEST_LINE_LEN} chars"
                )
            }
            Self::TooManyPanels { found, cap } => {
                write!(f, "too many persisted panels: {found}, cap {cap}")
            }
            Self::TitleTooLong { line } => write!(f, "manifest title too long at line {line}"),
        }
    }
}

impl std::error::Error for ManifestError {}

// ---------------------------------------------------------------------------
// PersistedPanel / RestartManifest
// ---------------------------------------------------------------------------

/// The restart record of one panel: identity, workspace index, title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPanel {
    /// The persisted identity (reused only under
    /// [`PersistencePolicy::StableIdentity`]).
    pub id: PanelId,
    /// Workspace index the panel belonged to.
    pub workspace: u32,
    /// Display title (`<=MAX_IDENTITY_TITLE_LEN` chars).
    pub title: String,
}

impl PersistedPanel {
    /// Builds a record, rejecting overlong titles fail-closed.
    pub fn new(id: PanelId, workspace: u32, title: &str) -> Result<Self, ManifestError> {
        if title.chars().count() > MAX_IDENTITY_TITLE_LEN {
            return Err(ManifestError::TitleTooLong { line: 0 });
        }
        Ok(Self {
            id,
            workspace,
            title: title.to_owned(),
        })
    }
}

/// The declarative restart record: existence only, no heap image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestartManifest {
    panels: Vec<PersistedPanel>,
}

impl RestartManifest {
    /// Creates an empty manifest.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one panel; fails closed past [`MAX_PERSIST_PANELS`].
    pub fn push(&mut self, panel: PersistedPanel) -> Result<(), ManifestError> {
        if self.panels.len() >= MAX_PERSIST_PANELS {
            return Err(ManifestError::TooManyPanels {
                found: self.panels.len() + 1,
                cap: MAX_PERSIST_PANELS,
            });
        }
        self.panels.push(panel);
        Ok(())
    }

    /// The recorded panels in insertion order.
    #[must_use]
    pub fn panels(&self) -> &[PersistedPanel] {
        &self.panels
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// Returns the number of recorded panels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }
}

// ---------------------------------------------------------------------------
// encode / decode
// ---------------------------------------------------------------------------

/// Encodes a manifest as newline-joined
/// `v=<n>;id=<n>;ws=<n>;title=<text>` records.
#[must_use]
pub fn encode_manifest(manifest: &RestartManifest) -> String {
    manifest
        .panels
        .iter()
        .map(|p| {
            format!(
                "v={};id={};ws={};title={}",
                PERSIST_MANIFEST_VERSION,
                p.id.get(),
                p.workspace,
                p.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decodes a manifest document, failing closed on the first defect.
///
/// Unknown `k=` fields are ignored (forward tolerance). Missing `id`,
/// `ws`, or `title` rejects the record; a `title` containing `;` or a
/// newline rejects the record (the encoding has no escaping by design —
/// titles with separators stay in memory only).
pub fn decode_manifest(doc: &str) -> Result<RestartManifest, ManifestError> {
    let mut out = RestartManifest::new();
    if doc.is_empty() {
        return Ok(out);
    }
    for (idx, line) in doc.lines().enumerate() {
        let no = idx + 1;
        if line.chars().count() > MAX_MANIFEST_LINE_LEN {
            return Err(ManifestError::LineTooLong { line: no });
        }
        let mut version: Option<u32> = None;
        let mut id: Option<u64> = None;
        let mut ws: Option<u32> = None;
        let mut title: Option<String> = None;
        for field in line.split(';') {
            let (k, v) = field
                .split_once('=')
                .ok_or(ManifestError::Malformed { line: no })?;
            match k {
                "v" => {
                    version = Some(
                        v.parse()
                            .map_err(|_| ManifestError::Malformed { line: no })?,
                    )
                }
                "id" => {
                    id = Some(
                        v.parse()
                            .map_err(|_| ManifestError::Malformed { line: no })?,
                    )
                }
                "ws" => {
                    ws = Some(
                        v.parse()
                            .map_err(|_| ManifestError::Malformed { line: no })?,
                    )
                }
                "title" => {
                    if v.contains('\n') {
                        return Err(ManifestError::Malformed { line: no });
                    }
                    title = Some(v.to_owned());
                }
                _ => {}
            }
        }
        match version {
            Some(PERSIST_MANIFEST_VERSION) => {}
            Some(found) => {
                return Err(ManifestError::UnsupportedVersion {
                    found,
                    supported: PERSIST_MANIFEST_VERSION,
                });
            }
            None => return Err(ManifestError::Malformed { line: no }),
        }
        let (Some(id), Some(workspace), Some(title)) = (id, ws, title) else {
            return Err(ManifestError::Malformed { line: no });
        };
        if title.contains(';') {
            return Err(ManifestError::Malformed { line: no });
        }
        if title.chars().count() > MAX_IDENTITY_TITLE_LEN {
            return Err(ManifestError::TitleTooLong { line: no });
        }
        if out.panels.len() >= MAX_PERSIST_PANELS {
            return Err(ManifestError::TooManyPanels {
                found: out.panels.len() + 1,
                cap: MAX_PERSIST_PANELS,
            });
        }
        out.panels.push(PersistedPanel {
            id: PanelId::new(id),
            workspace,
            title,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// plan_restore: deterministic re-attachment order
// ---------------------------------------------------------------------------

/// Restore order under [`PersistencePolicy::StableIdentity`]: the recorded
/// ids sorted ascending, so identical manifests re-attach identically.
///
/// Under [`PersistencePolicy::Ephemeral`] the caller mints fresh ids in
/// this same order and carries titles/workspace indices across; the plan
/// never mints ids itself (allocation stays with the runtime).
#[must_use]
pub fn plan_restore(manifest: &RestartManifest, policy: PersistencePolicy) -> Vec<PersistedPanel> {
    let mut plan = manifest.panels.clone();
    if policy == PersistencePolicy::StableIdentity {
        plan.sort_by_key(|p| p.id);
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RestartManifest {
        let mut m = RestartManifest::new();
        m.push(PersistedPanel::new(PanelId::new(9), 1, "logs").unwrap())
            .unwrap();
        m.push(PersistedPanel::new(PanelId::new(3), 0, "editor").unwrap())
            .unwrap();
        m
    }

    #[test]
    fn round_trip_preserves_records() {
        let doc = encode_manifest(&sample());
        let back = decode_manifest(&doc).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn stable_plan_sorts_by_id_ephemeral_keeps_order() {
        let m = sample();
        let stable = plan_restore(&m, PersistencePolicy::StableIdentity);
        assert_eq!(stable[0].id, PanelId::new(3));
        let ephemeral = plan_restore(&m, PersistencePolicy::Ephemeral);
        assert_eq!(ephemeral[0].id, PanelId::new(9));
    }

    #[test]
    fn bad_version_rejects_whole_document() {
        let err = decode_manifest("v=99;id=1;ws=0;title=x").unwrap_err();
        assert_eq!(
            err,
            ManifestError::UnsupportedVersion {
                found: 99,
                supported: PERSIST_MANIFEST_VERSION
            }
        );
    }
}
