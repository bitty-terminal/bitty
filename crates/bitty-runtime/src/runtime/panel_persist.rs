//! Durable producer and consumer of the panel restart manifest (bitty#3243).
//!
//! `bitty_ui::panel_persist` decided what a restart manifest *is* — the
//! newline-joined `v=2;id=…;ws=…;pid=…;title=…` record set with its
//! fail-closed decode — but until this module nothing in a shipped binary
//! produced one or read one back, so
//! [`Runtime::set_panel_restore_records`](crate::Runtime::set_panel_restore_records)
//! only ever saw test records and the restore planned zero panels.
//!
//! This module is that producer and consumer, and nothing else:
//!
//! - **Save.** [`Runtime::save_panel_manifest_to_path`] walks the installed panel
//!   host once
//!   ([`PanelRuntime::live_panel_records`](crate::registry::PanelRuntime::live_panel_records)),
//!   mints one durable record per live container, and commits
//!   [`bitty_ui::encode_manifest`]'s document to a sidecar path beside the
//!   session file. It uses the same
//!   [`SessionFileBackend`](super::session::SessionFileBackend) as the session
//!   bytes, so the write keeps the backend's atomic temp-plus-rename and size
//!   ceiling — Core never touches the filesystem directly.
//! - **Load.** [`Runtime::load_panel_manifest_from_path`] reads that document
//!   back, decodes it through [`bitty_ui::decode_manifest`], and configures the
//!   records the session restore plans. It runs before
//!   [`Runtime::apply_session_snapshot`], so the records reach
//!   [`Runtime::plan_panel_restore`](crate::runtime::panel_restore) instead of
//!   arriving after the step that consumes them.
//!
//! # Why a sidecar document
//!
//! The session file has its own versioned encoding (`bitty-storage`'s session
//! codec behind [`SessionFileBackend`](super::session::SessionFileBackend)) and
//! a consumer that cannot read it starts clean. Panel restore records are
//! *advisory identity* rather than session truth: under
//! [`PersistencePolicy::Ephemeral`](bitty_ui::PersistencePolicy) a restart
//! re-mints every id, so a manifest that is missing, stale, or unreadable costs
//! the panels' containers, not the session. Keeping it beside the session file
//! rather than inside it means the session codec's version gate and byte
//! ceiling are untouched by this slice, and the same argument that keeps
//! `RFC-OQ-9` open (`PERSIST_MANIFEST_VERSION` is the manifest's own version, at
//! `2`) stays the manifest's business.
//!
//! A missing, unreadable, or rejected document is a clean start **for panels
//! only**: the session restores, the panel step plans zero records, and the
//! caller sees it in [`PanelManifestLoad::present`]. A rejected *session* still
//! refuses the whole restore, unchanged.
//!
//! # What a record carries, and what it cannot
//!
//! The record's field set is `RFC-OQ-9`'s pending persistence contract
//! (bitty#3239 row 16), so this producer writes exactly the fields that already
//! exist: the advisory [`bitty_ui::PanelId`], the
//! `(WorkspaceId, PersistentId)` restore key, and the title. Two restore-time
//! facts are therefore *derived* here rather than persisted, and both spellings
//! are this slice's, not the RFC's:
//!
//! - **The container kind** comes from the durable key's prefix. The save side
//!   mints `PersistentId` as `<panel type>-<allocation id>` (`helper-7`), which
//!   is inside the key's `[a-z0-9_-]{1,64}` charset, so the load side can name
//!   the kind without a new field. A key whose prefix names no mintable kind is
//!   **not** restored: a record is not a data channel, and a container allocated
//!   as the wrong kind would present chrome for a panel nobody asked for.
//! - **The driver answer** comes from the port the composition root installs
//!   ([`crate::PanelRestoreDriverSource`]). With no port installed every record
//!   resolves to [`PanelDriverAnswer::Missing`], which is the shipped case: the
//!   container is restored and presents `unloaded` instead of the panel
//!   disappearing. Mapping a record to its installed plugin needs the owner
//!   field `RFC-OQ-9` has not decided yet, and that is the open half of
//!   bitty#3243.
//!
//! # Workspace ordinals
//!
//! A record's workspace component is the *persisted ordinal* the restore
//! resolves against the restored slot list (1-based, `ordinal - 1` into the
//! restored slots), never a live [`WorkspaceId`](crate::registry::WorkspaceId) —
//! the runtime mints those per window and a restart re-mints every one. The save
//! side therefore writes the index of the slot the container is attributed to,
//! `+ 1`, and skips a container attributed to no slot (window-global) or to a
//! workspace the live table does not hold.

use std::path::{Path, PathBuf};

use bitty_ui::{
    MAX_PERSIST_PANELS, PanelRestoreKey, PersistedPanel, PersistentId, RestartManifest,
    decode_manifest, encode_manifest,
};

use crate::registry::{LivePanelRecord, PanelType};

use super::Runtime;
use super::panel_restore::{PanelDriverAnswer, PanelRestoreBinding, PanelRestoreInput};
use super::session::SessionError;

/// Suffix appended to the session file's path for the panel manifest.
///
/// The manifest is a sibling document, never a second name for the session
/// file: `session.v3` keeps its codec and its version gate untouched, and
/// `session.v3.panels` carries the restart records.
pub const PANEL_MANIFEST_FILE_SUFFIX: &str = ".panels";

/// Path of the panel manifest beside `session_path`.
///
/// Pure path arithmetic — no filesystem access, so it is the same answer for a
/// save and for a load, and a backend that answers a session path from XDG
/// state gets the manifest beside it for free.
#[must_use]
pub fn panel_manifest_path(session_path: &Path) -> PathBuf {
    let mut raw = session_path.as_os_str().to_os_string();
    raw.push(PANEL_MANIFEST_FILE_SUFFIX);
    PathBuf::from(raw)
}

/// What one manifest save did (counts only; never record contents).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelManifestSave {
    /// A document was encoded and committed.
    Written {
        /// Records the document carried.
        records: usize,
        /// Document bytes committed.
        bytes: usize,
    },
    /// Nothing was observed because the panel host was borrowed elsewhere or
    /// the live set could not be represented as a complete document, so the
    /// previous document, if any, is untouched. A partial document is never
    /// written: a restore that silently lost records would present an
    /// incomplete workspace as a complete one.
    Unchanged,
}

/// What one manifest load found (counts only; never record contents).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanelManifestLoad {
    /// Whether a document was present and decoded. `false` covers a missing
    /// sidecar, an unreadable one, and one this build refuses (a version gate
    /// or a malformed record: the decoder fails closed on the whole document).
    pub present: bool,
    /// Records the document carried.
    pub records: usize,
    /// Records this runtime configured for the restore. Lower than
    /// [`Self::records`] when a record's durable key names no container kind
    /// this runtime mints.
    pub configured: usize,
}

impl Runtime {
    /// Encodes the current world's restart manifest and commits it beside the
    /// session file at `session_path`.
    ///
    /// Answers [`PanelManifestSave::Written`] with the record count and the
    /// document size; see [`PanelManifestSave`] for the one case that leaves the
    /// previous document in place.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] with no session backend installed (the same
    /// fail-closed answer the session save gives), and whatever the backend
    /// refuses: an over-ceiling payload or a filesystem failure. A failed
    /// commit leaves the previous destination untouched — the backend's own
    /// contract.
    pub fn save_panel_manifest_to_path(
        &self,
        session_path: &Path,
    ) -> Result<PanelManifestSave, SessionError> {
        let backend = self
            .session_backend
            .clone()
            .ok_or_else(|| SessionError::Io(String::from("no session backend")))?;
        let Some(manifest) = self.capture_restart_manifest() else {
            return Ok(PanelManifestSave::Unchanged);
        };
        let document = encode_manifest(&manifest);
        backend.commit_session_bytes(&panel_manifest_path(session_path), document.as_bytes())?;
        Ok(PanelManifestSave::Written {
            records: manifest.len(),
            bytes: document.len(),
        })
    }

    /// Reads the restart manifest beside `session_path` and configures the
    /// records the next session restore applies.
    ///
    /// Total and never fatal: a missing sidecar, an unreadable one, or one this
    /// build refuses configures nothing and answers
    /// [`PanelManifestLoad::default`], leaving the records a caller already
    /// configured alone. A document that *is* present replaces them, including
    /// with an empty record set — the world it describes had no panels, and
    /// keeping an older caller-configured set would resurrect containers the
    /// saved session did not have.
    ///
    /// Records are resolved to [`PanelRestoreInput`]s here, once per record,
    /// through the installed [`super::panel_restore::PanelRestoreDriverSource`].
    pub fn load_panel_manifest_from_path(&mut self, session_path: &Path) -> PanelManifestLoad {
        let Some(backend) = self.session_backend.clone() else {
            return PanelManifestLoad::default();
        };
        let bytes = match backend.load_session_bytes(&panel_manifest_path(session_path)) {
            Ok(bytes) => bytes,
            // Absent, over-ceiling, or unreadable: a clean start for panels.
            Err(_) => return PanelManifestLoad::default(),
        };
        let Ok(document) = std::str::from_utf8(&bytes) else {
            return PanelManifestLoad::default();
        };
        // The decoder fails closed on the whole document, so a single bad
        // record cannot half-restore a world.
        let Ok(manifest) = decode_manifest(document) else {
            return PanelManifestLoad::default();
        };
        let records = manifest.panels().len();
        let inputs: Vec<PanelRestoreInput> = manifest
            .panels()
            .iter()
            .filter_map(|record| self.panel_restore_input_of(record))
            .collect();
        let configured = inputs.len();
        self.set_panel_restore_records(inputs);
        PanelManifestLoad {
            present: true,
            records,
            configured,
        }
    }

    /// The durable record set of the world this runtime presents now.
    ///
    /// `None` when the manifest cannot be observed completely right now: the
    /// installed panel host is already borrowed (a save issued from inside a
    /// panel edge), or the live container set cannot be written inside
    /// [`MAX_PERSIST_PANELS`]. Neither is an error — the caller keeps the
    /// previous document rather than replacing it with a partial one.
    ///
    /// A runtime with no panel host is `Some(empty)`: there is no container to
    /// record, which is a complete observation, not a missing one.
    #[must_use]
    pub fn capture_restart_manifest(&self) -> Option<RestartManifest> {
        let mut manifest = RestartManifest::new();
        let Some(host) = self.scene_host.clone() else {
            return Some(manifest);
        };
        let Ok(host) = host.try_borrow() else {
            return None;
        };
        let live = host.live_panel_records();
        // The document's cap is the manifest's, not the registry's: a live set
        // the record grammar cannot hold whole is not written at all.
        if live.len() > MAX_PERSIST_PANELS {
            return None;
        }
        for record in live {
            let Some(persisted) = self.persisted_panel_of(&record) else {
                continue;
            };
            if manifest.push(persisted).is_err() {
                return None;
            }
        }
        Some(manifest)
    }

    /// One live container as a durable record, or `None` when it has no
    /// representable identity.
    fn persisted_panel_of(&self, record: &LivePanelRecord) -> Option<PersistedPanel> {
        // Terminal containers are the PTY path's: process-spawn authority stays
        // with the session's pane sessions, and a manifest never revives one.
        if record.panel_type == PanelType::Terminal {
            return None;
        }
        // A window-global container (no workspace attribution) has no ordinal to
        // restore into, and a record may not invent one.
        let workspace = record.workspace?;
        let ordinal = self.persisted_ordinal_of(workspace)?;
        let persistent = persistent_id_of(record)?;
        let title = encodable_title(record.title.as_deref(), persistent.as_str()).to_owned();
        PersistedPanel::new(
            bitty_ui::PanelId::new(record.id.0),
            PanelRestoreKey::new(bitty_ui::WorkspaceId::new(ordinal), persistent),
            &title,
        )
        .ok()
    }

    /// The 1-based persisted ordinal of the live slot `workspace`.
    ///
    /// A restore resolves a record's ordinal as `ordinal - 1` into the restored
    /// slot list, so the save side writes the index the container's workspace
    /// occupies now, `+ 1`. `None` when the live table does not hold it.
    fn persisted_ordinal_of(&self, workspace: crate::registry::WorkspaceId) -> Option<u64> {
        let index = self
            .workspaces
            .iter()
            .position(|slot| slot.id == workspace)?;
        u64::try_from(index).ok().map(|index| index + 1)
    }

    /// Resolves one decoded record to the restore input the panel step applies.
    fn panel_restore_input_of(&self, record: &PersistedPanel) -> Option<PanelRestoreInput> {
        let binding = match self.panel_restore_driver_source.as_ref() {
            Some(source) => source.resolve(record),
            // No port installed: the shipped case. The kind comes from the
            // durable key's prefix and the driver is simply absent, so the
            // container comes back presenting `unloaded`.
            None => PanelRestoreBinding::new(
                panel_type_of_persistent_id(&record.key.persistent)?,
                PanelDriverAnswer::Missing,
            ),
        };
        Some(PanelRestoreInput::new(
            record.clone(),
            binding.panel_type,
            binding.driver,
        ))
    }
}

/// The durable key this producer mints for one live container.
///
/// `<panel type>-<allocation id>`, inside the key's `[a-z0-9_-]{1,64}` charset.
/// The allocation id is *this* process's handle, not an identity that survives a
/// restart: under `PersistencePolicy::Ephemeral` a restore mints a fresh
/// container, so the next save mints the key from the new handle. The prefix is
/// the only place the container kind is recorded, because the record's field set
/// is `RFC-OQ-9`'s (bitty#3239 row 16) and a type field would be that RFC's
/// change, not this producer's.
fn persistent_id_of(record: &LivePanelRecord) -> Option<PersistentId> {
    PersistentId::new(format!("{}-{}", record.panel_type.as_str(), record.id.0)).ok()
}

/// The container kind a durable key names, if this runtime mints one.
fn panel_type_of_persistent_id(persistent: &PersistentId) -> Option<PanelType> {
    let (prefix, _) = persistent.as_str().split_once('-')?;
    PanelType::parse(prefix).filter(|panel_type| *panel_type != PanelType::Terminal)
}

/// A title the manifest's record grammar can carry.
///
/// The encoding has no escaping by design (`bitty_ui::decode_manifest` refuses a
/// `;` inside a text field), so a stored title carrying the field separator or a
/// newline stays in memory and the durable title falls back to the key. An
/// overlong title is truncated at a character boundary to the identity bound
/// rather than dropped, because a truncated title is still the panel's name.
fn encodable_title<'a>(raw: Option<&'a str>, persistent: &'a str) -> &'a str {
    let Some(raw) = raw else {
        return persistent;
    };
    if raw.is_empty() || raw.contains(';') || raw.contains('\n') {
        return persistent;
    }
    if raw.chars().count() <= bitty_ui::MAX_IDENTITY_TITLE_LEN {
        return raw;
    }
    match raw.char_indices().nth(bitty_ui::MAX_IDENTITY_TITLE_LEN) {
        Some((end, _)) => &raw[..end],
        None => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_path_is_the_session_path_with_one_suffix() {
        assert_eq!(
            panel_manifest_path(Path::new("/state/bitty/session.v3")),
            PathBuf::from("/state/bitty/session.v3.panels")
        );
        // A path with no parent and no extension still gets its own name.
        assert_eq!(
            panel_manifest_path(Path::new("session")),
            PathBuf::from("session.panels")
        );
    }

    #[test]
    fn a_durable_key_round_trips_its_container_kind() {
        let persistent = PersistentId::new("helper-7").expect("valid key");
        assert_eq!(
            panel_type_of_persistent_id(&persistent),
            Some(PanelType::Helper)
        );
        // A key with no prefix names no kind, and terminal is never restored as
        // a plugin container.
        assert_eq!(
            panel_type_of_persistent_id(&PersistentId::new("logs").expect("valid key")),
            None
        );
        assert_eq!(
            panel_type_of_persistent_id(&PersistentId::new("terminal-2").expect("valid key")),
            None
        );
    }

    #[test]
    fn a_title_the_record_grammar_cannot_carry_falls_back_to_the_key() {
        assert_eq!(encodable_title(Some("logs"), "helper-7"), "logs");
        assert_eq!(encodable_title(None, "helper-7"), "helper-7");
        assert_eq!(encodable_title(Some("a;b"), "helper-7"), "helper-7");
        assert_eq!(encodable_title(Some("a\nb"), "helper-7"), "helper-7");
        assert_eq!(encodable_title(Some(""), "helper-7"), "helper-7");
        let long = "x".repeat(bitty_ui::MAX_IDENTITY_TITLE_LEN + 10);
        assert_eq!(
            encodable_title(Some(&long), "helper-7").chars().count(),
            bitty_ui::MAX_IDENTITY_TITLE_LEN
        );
    }
}
