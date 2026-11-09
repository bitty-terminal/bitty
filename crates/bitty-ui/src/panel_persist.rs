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
//!   record: which panels existed, with which title and restore key. A
//!   manifest carries no heap image, no pointer, and no allocator state.
//! - [`encode_manifest`] / [`decode_manifest`] — the newline-joined
//!   `v=<n>;id=<n>;ws=<n>;pid=<text>;title=<text>` document spelling with the
//!   same fail-closed policy as panel snapshots: unknown `k=` fields are
//!   ignored (forward tolerance), a record pinned to an unsupported `v=`
//!   rejects the whole document, and a malformed record rejects the whole
//!   document with nothing applied.
//! - [`plan_restore`] — the restore entry point. It refuses
//!   [`PersistencePolicy::StableIdentity`] explicitly: a [`PanelId`] is not a
//!   restore key, so restart re-mints identity and the manifest carries the
//!   [`PanelRestoreKey`] `(WorkspaceId, PersistentId)` instead.
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
use crate::targeting::WorkspaceId;

/// Manifest format version written by [`encode_manifest`].
///
/// Version 2 carries the `(WorkspaceId, PersistentId)` restore key. Version 1
/// stored a bare workspace index, which is not an identity and cannot key a
/// restore, so a version-1 document is refused rather than reinterpreted.
pub const PERSIST_MANIFEST_VERSION: u32 = 2;

/// Hard cap in bytes on a [`PersistentId`].
///
/// Mirrors the registry's terminal persistent-id bound: `1..=64` bytes over
/// the charset `[a-z0-9_-]`.
pub const MAX_PERSISTENT_ID_LEN: usize = 64;

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
    /// A persistent id is empty, overlong, or outside `[a-z0-9_-]`.
    InvalidPersistentId {
        /// 1-based line number in the submitted document (`0` for an
        /// in-memory constructor call).
        line: usize,
    },
    /// The caller selected [`PersistencePolicy::StableIdentity`] at the
    /// restore entry point, which the panel rules refuse: a [`PanelId`] is not
    /// a restore key.
    StableIdentityUnsupported,
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
            Self::InvalidPersistentId { line } => {
                write!(f, "invalid persistent id at line {line}")
            }
            Self::StableIdentityUnsupported => {
                f.write_str("stable-identity restore is refused: a panel id is not a restore key")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

// ---------------------------------------------------------------------------
// PersistentId / PanelRestoreKey
// ---------------------------------------------------------------------------

/// Stable cross-restart identity component for a panel (OQ-084 B.4).
///
/// Bounded to `1..=MAX_PERSISTENT_ID_LEN` bytes over the charset
/// `[a-z0-9_-]`. A [`PanelId`] is an allocation handle and is never a restore
/// key; this id is what a manifest carries so restart can re-mint the
/// [`PanelId`] while keeping the workspace/panel pairing.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PersistentId(String);

impl PersistentId {
    /// Validates and creates a persistent id.
    ///
    /// # Errors
    /// [`ManifestError::InvalidPersistentId`] when empty, overlong, or
    /// outside the charset.
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestError> {
        let value = value.into();
        Self::validate(&value).map_err(|_| ManifestError::InvalidPersistentId { line: 0 })?;
        Ok(Self(value))
    }

    fn validate(value: &str) -> Result<(), ()> {
        if value.is_empty() || value.len() > MAX_PERSISTENT_ID_LEN {
            return Err(());
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(());
        }
        Ok(())
    }

    /// The canonical spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PersistentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `(WorkspaceId, PersistentId)` restore key (OQ-084 B.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PanelRestoreKey {
    /// Workspace the panel belonged to.
    pub workspace: WorkspaceId,
    /// Stable panel identity inside that workspace.
    pub persistent: PersistentId,
}

impl PanelRestoreKey {
    /// Builds a key from its two identity components.
    #[must_use]
    pub fn new(workspace: WorkspaceId, persistent: PersistentId) -> Self {
        Self {
            workspace,
            persistent,
        }
    }
}

// ---------------------------------------------------------------------------
// PersistedPanel / RestartManifest
// ---------------------------------------------------------------------------

/// The restart record of one panel: allocation handle, restore key, title.
///
/// `id` is advisory only — it is the id the panel had before the restart and
/// is never used to re-attach. The [`PanelRestoreKey`] is what identifies the
/// record across restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPanel {
    /// The persisted allocation handle (advisory; restart re-mints identity).
    pub id: PanelId,
    /// The `(WorkspaceId, PersistentId)` restore key.
    pub key: PanelRestoreKey,
    /// Display title (`<=MAX_IDENTITY_TITLE_LEN` chars).
    pub title: String,
}

impl PersistedPanel {
    /// Builds a record, rejecting overlong titles fail-closed.
    pub fn new(id: PanelId, key: PanelRestoreKey, title: &str) -> Result<Self, ManifestError> {
        if title.chars().count() > MAX_IDENTITY_TITLE_LEN {
            return Err(ManifestError::TitleTooLong { line: 0 });
        }
        Ok(Self {
            id,
            key,
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
/// `v=<n>;id=<n>;ws=<n>;pid=<text>;title=<text>` records.
#[must_use]
pub fn encode_manifest(manifest: &RestartManifest) -> String {
    manifest
        .panels
        .iter()
        .map(|p| {
            format!(
                "v={};id={};ws={};pid={};title={}",
                PERSIST_MANIFEST_VERSION,
                p.id.get(),
                p.key.workspace.get(),
                p.key.persistent.as_str(),
                p.title
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decodes a manifest document, failing closed on the first defect.
///
/// Unknown `k=` fields are ignored (forward tolerance). Missing `id`,
/// `ws`, `pid`, or `title` rejects the record; an invalid `pid`, a `title`
/// containing `;`, or a newline in either text field rejects the record (the
/// encoding has no escaping by design — titles with separators stay in memory
/// only). A version-1 document is refused: it carries a workspace index, not
/// the `(WorkspaceId, PersistentId)` key.
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
        let mut ws: Option<u64> = None;
        let mut pid: Option<String> = None;
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
                "pid" => {
                    if v.contains('\n') {
                        return Err(ManifestError::Malformed { line: no });
                    }
                    pid = Some(v.to_owned());
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
        let (Some(id), Some(workspace), Some(persistent), Some(title)) = (id, ws, pid, title)
        else {
            return Err(ManifestError::Malformed { line: no });
        };
        if title.contains(';') || persistent.contains(';') {
            return Err(ManifestError::Malformed { line: no });
        }
        PersistentId::validate(&persistent)
            .map_err(|()| ManifestError::InvalidPersistentId { line: no })?;
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
            key: PanelRestoreKey::new(WorkspaceId::new(workspace), PersistentId(persistent)),
            title,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// plan_restore: the panel restore entry point
// ---------------------------------------------------------------------------

/// The panel restore entry point.
///
/// [`PersistencePolicy::StableIdentity`] is refused explicitly (OQ-084 B.5):
/// a [`PanelId`] is an allocation handle, not a restore key, so no caller may
/// ask the manifest to resurrect one. Under
/// [`PersistencePolicy::Ephemeral`] the caller mints fresh ids in manifest
/// order and carries the [`PanelRestoreKey`] and title across; the plan never
/// mints ids itself (allocation stays with the runtime).
///
/// # Errors
/// [`ManifestError::StableIdentityUnsupported`] for the refused policy.
pub fn plan_restore(
    manifest: &RestartManifest,
    policy: PersistencePolicy,
) -> Result<Vec<PersistedPanel>, ManifestError> {
    if policy == PersistencePolicy::StableIdentity {
        return Err(ManifestError::StableIdentityUnsupported);
    }
    Ok(manifest.panels.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(workspace: u64, persistent: &str) -> PanelRestoreKey {
        PanelRestoreKey::new(
            WorkspaceId::new(workspace),
            PersistentId::new(persistent).unwrap(),
        )
    }

    fn sample() -> RestartManifest {
        let mut m = RestartManifest::new();
        m.push(PersistedPanel::new(PanelId::new(9), key(1, "logs"), "logs").unwrap())
            .unwrap();
        m.push(PersistedPanel::new(PanelId::new(3), key(0, "editor"), "editor").unwrap())
            .unwrap();
        m
    }

    #[test]
    fn round_trip_preserves_records() {
        let doc = encode_manifest(&sample());
        let back = decode_manifest(&doc).unwrap();
        assert_eq!(back, sample());
        assert_eq!(back.panels()[0].key.workspace, WorkspaceId::new(1));
        assert_eq!(back.panels()[0].key.persistent.as_str(), "logs");
    }

    /// OQ-084 B.5: the restore entry point refuses `StableIdentity`.
    #[test]
    fn stable_identity_is_refused_at_the_restore_entry() {
        assert_eq!(
            plan_restore(&sample(), PersistencePolicy::StableIdentity),
            Err(ManifestError::StableIdentityUnsupported)
        );
        let ephemeral = plan_restore(&sample(), PersistencePolicy::Ephemeral).unwrap();
        assert_eq!(ephemeral[0].id, PanelId::new(9));
        assert_eq!(ephemeral[1].id, PanelId::new(3));
    }

    /// A version-1 document carried a workspace index, not a restore key.
    #[test]
    fn version_one_documents_are_refused() {
        assert_eq!(
            decode_manifest("v=1;id=1;ws=0;title=x").unwrap_err(),
            ManifestError::UnsupportedVersion {
                found: 1,
                supported: PERSIST_MANIFEST_VERSION
            }
        );
    }

    #[test]
    fn invalid_persistent_ids_are_refused() {
        for doc in [
            "v=2;id=1;ws=0;pid=;title=x",
            "v=2;id=1;ws=0;pid=BAD;title=x",
            "v=2;id=1;ws=0;pid=has space;title=x",
        ] {
            assert_eq!(
                decode_manifest(doc).unwrap_err(),
                ManifestError::InvalidPersistentId { line: 1 }
            );
        }
        assert!(PersistentId::new("").is_err());
        assert!(PersistentId::new("x".repeat(MAX_PERSISTENT_ID_LEN + 1)).is_err());
        assert!(PersistentId::new("editor-1_ok").is_ok());
    }

    #[test]
    fn bad_version_rejects_whole_document() {
        let err = decode_manifest("v=99;id=1;ws=0;pid=x;title=x").unwrap_err();
        assert_eq!(
            err,
            ManifestError::UnsupportedVersion {
                found: 99,
                supported: PERSIST_MANIFEST_VERSION
            }
        );
    }
}
