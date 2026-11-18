//! Panel restore on the session-restore path (bitty#3239 row 14).
//!
//! [`plan_restore`] decided how a persisted panel record is re-attached, but
//! until this module it had no production caller: the session restore rebuilt
//! workspaces and panes only, so a panel whose driver is missing, denied, or
//! older could never reach its bounded outcome. The restore now applies panel
//! records the same way it applies panes — inside
//! [`Runtime::apply_session_snapshot`], after validation and before the first
//! painted frame.
//!
//! # What the step does
//!
//! 1. **Plans before any mutation.** Every configured record is pushed into a
//!    [`RestartManifest`] and passed through [`plan_restore`] under the
//!    configured [`PersistencePolicy`]. `StableIdentity` is refused there
//!    ([`ManifestError::StableIdentityUnsupported`]) and the whole restore is
//!    refused before a layout, a focus, or a pane session is touched — the
//!    atomicity contract the session path already keeps. A record set over
//!    [`bitty_ui::MAX_PERSIST_PANELS`] refuses the same way.
//! 2. **Mints a fresh [`PanelId`] per record.** The restored container is
//!    allocated by [`PanelRuntime::create_panel`], so identity comes from the
//!    registry and never from the record. The record's `id` field is advisory
//!    (it is the id the panel had before the restart): it is reported back so a
//!    caller can correlate, and it is never used to re-attach. The record's
//!    [`PanelRestoreKey`] and title travel into the report unchanged.
//! 3. **Binds, or presents the bounded outcome.** A record whose driver is
//!    installed takes a first bind — no predecessor, no ceiling, the whole
//!    resolution granted, because `panel.create` is the authorization. A record
//!    whose driver is missing, denied, or older keeps its container and presents
//!    the Core-owned placeholder for that reason (`unloaded`, `denied`,
//!    `incompatible`).
//! 4. **Bounds the rest.** A record whose workspace names no slot in the
//!    restored session, or that the registry refuses to allocate or bind, is
//!    dropped with a typed reason instead of being retried or silently
//!    orphaned: a container allocated for a record that then failed is disposed
//!    again, so no record leaves half a panel behind.
//!
//! # What this module does not decide
//!
//! The driver answer is an input, not a probe. Which plugin is installed, which
//! capability a permission denies, and whether a persisted record was written
//! under a newer schema than the installed provider supports are facts the
//! plugin host owns; Core owns only the bounded outcome. The persisted record
//! still carries no creating owner, no panel type, and no provider contract
//! version (bitty#3239 rows 13 and 16 stay open, and the record's field set is
//! `RFC-OQ-9`'s pending persistence contract), so this module asks for exactly
//! the three facts the record cannot carry and decides nothing about them.
//!
//! The workspace side resolves through the persisted workspace ordinal. A
//! record's [`PanelRestoreKey`] names the workspace the panel belonged to by
//! the ordinal that workspace occupied in the persisted session (1-based — the
//! `ws:N` index the window presents, and the origin the slot minter starts
//! from), while [`Runtime`] mints a fresh [`WorkspaceId`] for every restored
//! slot: a slot id is per-window attribution and no restored slot ever keeps
//! the id it had. The step therefore resolves the ordinal against the restored
//! slot list — `ordinal - 1`, bounded by the number of slots the snapshot
//! restores — and restores the container into that slot's fresh id. Resolution
//! is never by raw value against the live slots: after a restart the persisted
//! value names no live id, so every record would drop on every window but the
//! one that happened to mint that number. A record whose ordinal falls outside
//! the restored session is dropped, never re-homed onto another workspace.
//!
//! # The outgoing world
//!
//! A restore replaces the whole slot table, so the ids of the world it replaces
//! die with it. The step releases the containers attributed to those outgoing
//! slots before the restored world is installed, so no container is left
//! attributed to a workspace that no longer exists — an attribution no later
//! workspace edge could reach and no later close sweep would ever visit.
//! Containers created without a workspace attribution are window-global and are
//! left alone.
//!
//! # Lifetime of the records
//!
//! The configured records are consumed by the restore that applies them
//! ([`Runtime::set_panel_restore_records`]), so replaying a restore cannot mint
//! a second container for a record that already has one. A refused restore
//! leaves them in place, and the per-record outcome stays readable through
//! [`Runtime::panel_restore_report`] afterwards.

use std::collections::BTreeSet;

use bitty_plugin_host::PluginId;
use bitty_ui::panel_recovery::PlaceholderReason;
use bitty_ui::{
    ManifestError, PanelRestoreKey, PersistedPanel, PersistencePolicy, RestartManifest,
    plan_restore,
};

use crate::registry::{
    PanelError, PanelHandle, PanelId, PanelRuntime, PanelState, PanelType, WorkspaceId,
};

use super::Runtime;

// ---------------------------------------------------------------------------
// Inputs: the record plus the facts it cannot carry
// ---------------------------------------------------------------------------

/// What the plugin host answers about a persisted record's driver at restore.
///
/// The answer is the host's; this module only turns it into the bounded
/// outcome Decision E names. It is not a probe of plugin state and carries no
/// plugin text: an `Installed` answer brings the driver's own resolution of the
/// `panel.*` capabilities, which is what a first bind commits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelDriverAnswer {
    /// A driver is installed and available for this container.
    Installed {
        /// The plugin that will drive the container.
        owner: PluginId,
        /// The driver's own resolution of the `panel.*` capabilities. A first
        /// bind commits it whole: there is no predecessor ceiling.
        resolution: BTreeSet<String>,
        /// Capabilities a permission-diff consent admits for this binding only.
        consented: BTreeSet<String>,
    },
    /// Nothing installed serves the record: the container keeps an `unloaded`
    /// placeholder (Decision E rule 4 — unloaded, not deleted).
    Missing,
    /// A driver is installed but a required capability is denied (safe mode
    /// included): the container presents `denied` and is `unavailable`.
    Denied,
    /// A driver is installed but its panel contract is older than the
    /// persisted record: nothing is downgraded or partially applied, and the
    /// container presents `incompatible`.
    Older,
}

/// One persisted panel record plus the restore-time facts it cannot carry.
///
/// The composition root configures these on the runtime
/// ([`Runtime::set_panel_restore_records`]) before the session restore runs;
/// the restore path applies them in the order they are given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelRestoreInput {
    /// The durable record: advisory [`PanelId`], `(WorkspaceId, PersistentId)`
    /// restore key, title.
    pub record: PersistedPanel,
    /// Container kind the record restores as. The record carries no type
    /// (bitty#3239 row 16 keeps the field set open), so the driver answer
    /// brings the one thing the registry needs to allocate the container.
    pub panel_type: PanelType,
    /// What the plugin host resolves the record's driver to.
    pub driver: PanelDriverAnswer,
}

impl PanelRestoreInput {
    /// Builds one restore input.
    #[must_use]
    pub fn new(record: PersistedPanel, panel_type: PanelType, driver: PanelDriverAnswer) -> Self {
        Self {
            record,
            panel_type,
            driver,
        }
    }
}

// ---------------------------------------------------------------------------
// Report: what one restore did with each record
// ---------------------------------------------------------------------------

/// Why a persisted panel did not come back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelRestoreDrop {
    /// No panel host is installed in this runtime, so there is no registry to
    /// restore a container into.
    NoPanelHost,
    /// The installed panel host was already borrowed for the whole step, so
    /// nothing could be allocated. The record is not retried inside the restore;
    /// the next restore that finds the host free applies it.
    PanelHostBusy,
    /// The record's workspace names no slot in the restored session. The
    /// record is not re-homed: an unresolvable workspace is a persistence
    /// question, not something a restore may guess.
    WorkspaceGone,
    /// The registry refused the allocation or the bind (capacity, generation
    /// exhaustion, a refused fence). A container allocated for the record is
    /// disposed again before the drop is reported.
    RegistryRefused(PanelError),
}

/// What one persisted record restored to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelRestoreOutcome {
    /// A first bind committed on a fresh container.
    Bound {
        /// Freshly minted container id.
        panel: PanelId,
        /// Restored workspace the container belongs to.
        workspace: WorkspaceId,
        /// The committed grant set: the driver's whole resolution.
        grants: BTreeSet<String>,
    },
    /// The container is retained and presents the Core-owned placeholder.
    Placeholder {
        /// Freshly minted container id.
        panel: PanelId,
        /// Restored workspace the container belongs to.
        workspace: WorkspaceId,
        /// The reason the container presents.
        reason: PlaceholderReason,
    },
    /// Nothing came back for this record.
    Dropped {
        /// Why the record was bounded out.
        reason: PanelRestoreDrop,
    },
}

/// One record's restore result: the durable identity carried across plus what
/// the restore did with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredPanel {
    /// The id the record carried. Advisory: never used to re-attach, reported
    /// only so a caller can correlate a record with the container it became.
    pub persisted_id: PanelId,
    /// The `(WorkspaceId, PersistentId)` restore key, carried verbatim.
    pub key: PanelRestoreKey,
    /// Display title, carried verbatim.
    pub title: String,
    /// What the restore did with the record.
    pub outcome: PanelRestoreOutcome,
}

/// Outcome of the panel step of one session restore (content-free: ids, keys,
/// titles, and the bounded outcome, never panel content).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelRestoreReport {
    panels: Vec<RestoredPanel>,
}

impl PanelRestoreReport {
    /// One entry per configured record, in configuration order.
    #[must_use]
    pub fn panels(&self) -> &[RestoredPanel] {
        &self.panels
    }

    /// Number of records the step considered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    /// Whether the step considered no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }

    /// Records that took a first bind.
    #[must_use]
    pub fn bound(&self) -> usize {
        self.count(|outcome| matches!(outcome, PanelRestoreOutcome::Bound { .. }))
    }

    fn count(&self, predicate: impl Fn(&PanelRestoreOutcome) -> bool) -> usize {
        self.panels
            .iter()
            .filter(|panel| predicate(&panel.outcome))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Planning (pure, pre-mutation)
// ---------------------------------------------------------------------------

/// One record after planning: the record itself plus the slot it lands in.
///
/// Reachable at `pub(in crate::runtime)` so the session-restore path
/// ([`Runtime::apply_session_snapshot`]) can carry a plan from the pre-mutation
/// half of the step to the commit; its fields stay private to this module, so
/// nothing outside can forge or reorder a plan.
pub(super) struct PlannedPanel {
    record: PersistedPanel,
    panel_type: PanelType,
    driver: PanelDriverAnswer,
    /// Index into the restored slot list, or `None` when the record's
    /// workspace names no slot being restored.
    slot: Option<usize>,
}

impl Runtime {
    /// Selects the [`PersistencePolicy`] panel records are planned under at the
    /// next session restore (bitty#3239 row 14).
    ///
    /// The default is [`PersistencePolicy::Ephemeral`], the fail-safe outcome
    /// the persistence module documents while `RFC-OQ-9` is open: restart
    /// re-mints identity rather than resurrecting a persisted one. Selecting
    /// [`PersistencePolicy::StableIdentity`] makes every subsequent restore
    /// refuse, because a [`PanelId`] is not a restore key.
    pub fn set_panel_persistence_policy(&mut self, policy: PersistencePolicy) {
        self.panel_restore_policy = policy;
    }

    /// Configures the panel records the next session restore applies.
    ///
    /// Records are applied in the order given and are consumed by the restore
    /// that applies them: after a successful restore this list is empty again,
    /// so replaying a restore cannot mint a second container for a record that
    /// already has one. A refused restore leaves them in place, so a caller can
    /// correct the policy and retry.
    ///
    /// They are not persisted by this call and the restore never writes them:
    /// the durable store stays the session backend's, and the record encoding is
    /// untouched by this slice.
    pub fn set_panel_restore_records(&mut self, records: Vec<PanelRestoreInput>) {
        self.panel_restore_inputs = records;
    }

    /// The panel records the next session restore will apply (empty once a
    /// restore consumed them).
    #[must_use]
    pub fn panel_restore_records(&self) -> &[PanelRestoreInput] {
        &self.panel_restore_inputs
    }

    /// What the last successful session restore did with the configured panel
    /// records (empty before any restore, and reset by a refused one).
    #[must_use]
    pub fn panel_restore_report(&self) -> &PanelRestoreReport {
        &self.panel_restore_report
    }

    /// Plans the panel step of a restore that is about to rebuild
    /// `restored_slots` workspaces.
    ///
    /// Pure: nothing here touches the layout, the focus, a pane session, or the
    /// panel registry. It is the pre-mutation half of the step, so a refusal
    /// ([`ManifestError`]) leaves the runtime exactly as it was.
    pub(super) fn plan_panel_restore(
        &self,
        restored_slots: usize,
    ) -> Result<Vec<PlannedPanel>, ManifestError> {
        let mut manifest = RestartManifest::new();
        for input in &self.panel_restore_inputs {
            // Over-cap record sets fail closed here, before any mutation, with
            // the same typed error a decoded document would produce.
            manifest.push(input.record.clone())?;
        }
        // The restore entry point: `StableIdentity` is refused here.
        let records = plan_restore(&manifest, self.panel_restore_policy)?;
        let mut planned = Vec::with_capacity(records.len());
        for (index, record) in records.into_iter().enumerate() {
            let Some(input) = self.panel_restore_inputs.get(index) else {
                continue;
            };
            planned.push(PlannedPanel {
                slot: restored_slot_of(record.key.workspace.get(), restored_slots),
                record,
                panel_type: input.panel_type,
                driver: input.driver.clone(),
            });
        }
        Ok(planned)
    }

    /// Releases every container attributed to a workspace of the outgoing world.
    ///
    /// A restore replaces the whole slot table, and a slot [`WorkspaceId`] is a
    /// per-window attribution the minter never reuses, so the ids of the world
    /// being replaced die with it. A container still attributed to one of them
    /// would be unreachable: no restored workspace resolves to that id, and
    /// every later sweep (the one a workspace close runs) walks live slots
    /// only, so nothing would ever dispose it and the registry would keep
    /// counting it. The step therefore runs the same per-workspace disposal a
    /// workspace close runs, for every outgoing slot, before the restored world
    /// is installed.
    ///
    /// Containers created without a workspace attribution are window-global and
    /// are left alone: no dead slot id is involved, so nothing orphans them. A
    /// host the composition root is holding across the restore cannot be
    /// borrowed, and the release is skipped for that restore — the same bounded
    /// borrow the panel step itself takes (`PanelHostBusy`), never a panic
    /// inside Core.
    pub(super) fn release_panels_of_outgoing_world(&mut self) {
        // Collected first: the sweep needs the host mutably, and the ids must
        // be the outgoing ones (the caller replaces the table right after).
        let outgoing: Vec<WorkspaceId> = self.workspaces.iter().map(|slot| slot.id).collect();
        self.with_scene_host_mut(|host| {
            for workspace in outgoing {
                // The sweep is fail-fast and reports what it could not release.
                // A refusal there is a registry invariant violation (the sweep
                // disposes each container at its own allocation epoch, so a
                // stale handle cannot arise), and the world it belonged to is
                // already gone from the layout domain, so there is no second
                // path to retry it on.
                let _ = host.dispose_workspace_panels(workspace);
            }
        });
    }

    /// Commits the planned panel step after the restored world is installed.
    ///
    /// Bounded and total: every planned record yields exactly one report entry,
    /// and a record that cannot be restored is dropped with a typed reason
    /// (after disposing the container it may have allocated) rather than
    /// aborting a restore whose layout, focus, and sessions are already in
    /// place.
    pub(super) fn commit_panel_restore(
        &mut self,
        planned: Vec<PlannedPanel>,
    ) -> PanelRestoreReport {
        let host = self.scene_host.clone();
        let mut panels = Vec::with_capacity(planned.len());
        for item in planned {
            let outcome = self.restore_one_panel(host.as_ref(), &item);
            panels.push(RestoredPanel {
                persisted_id: item.record.id,
                key: item.record.key,
                title: item.record.title,
                outcome,
            });
        }
        PanelRestoreReport { panels }
    }

    /// Restores one planned record: allocate a fresh container, then bind it or
    /// present its placeholder.
    fn restore_one_panel(
        &self,
        host: Option<&std::rc::Rc<std::cell::RefCell<PanelRuntime>>>,
        item: &PlannedPanel,
    ) -> PanelRestoreOutcome {
        let Some(slot) = item.slot else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::WorkspaceGone,
            };
        };
        let Some(slot) = self.workspaces.get(slot) else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::WorkspaceGone,
            };
        };
        let Some(host) = host else {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::NoPanelHost,
            };
        };
        let workspace = slot.id;
        // `try_borrow_mut`: a caller holding the shared handle across the
        // restore is a bounded drop, never a panic inside Core.
        let mut host = match host.try_borrow_mut() {
            Ok(host) => host,
            Err(_) => {
                return PanelRestoreOutcome::Dropped {
                    reason: PanelRestoreDrop::PanelHostBusy,
                };
            }
        };
        // The container is allocated here, so its `PanelId` is fresh: the
        // record's advisory id was never a restore key and is never passed in.
        let handle = match host.create_panel(item.panel_type, Some(workspace)) {
            Ok(handle) => handle,
            Err(error) => {
                return PanelRestoreOutcome::Dropped {
                    reason: PanelRestoreDrop::RegistryRefused(error),
                };
            }
        };
        let outcome = match &item.driver {
            PanelDriverAnswer::Installed {
                owner,
                resolution,
                consented,
            } => bind_restored_panel(&mut host, handle, workspace, owner, resolution, consented),
            PanelDriverAnswer::Missing => {
                present_restored_placeholder(&mut host, handle, workspace, "unloaded")
            }
            PanelDriverAnswer::Denied => {
                present_restored_placeholder(&mut host, handle, workspace, "denied")
            }
            PanelDriverAnswer::Older => {
                present_restored_placeholder(&mut host, handle, workspace, "incompatible")
            }
        };
        if matches!(outcome, PanelRestoreOutcome::Dropped { .. }) {
            // Never leave a container the record did not earn: the join between
            // allocation and presentation is where a half-restored panel could
            // otherwise survive as an orphan.
            let _ = host.dispose_panel(handle.id, handle.generation);
        }
        outcome
    }
}

/// Resolves a record's persisted workspace ordinal to the restored slot it
/// lands in.
///
/// A record's key carries [`bitty_ui::WorkspaceId`] — the targeting newtype —
/// while the runtime names slots with [`WorkspaceId`]. The two are distinct
/// types over the same number, but the number is not the same fact in each: the
/// record's value is the *persisted workspace ordinal* (1-based, the `ws:N`
/// index the window presents and the origin the slot minter starts from), and a
/// live slot id is per-window attribution that the minter mints fresh for every
/// restored slot. Matching the raw value against the live slots — what this
/// step used to do — resolves at most the one workspace the starting window
/// happened to mint, so every record from a second or later workspace of the
/// persisted session dropped as `WorkspaceGone`; in the production shape (one
/// workspace at startup) that is every record but the first workspace's.
///
/// Resolution is therefore by the ordinal, against the slots the snapshot is
/// about to restore: `ordinal - 1` for the zero-based slot list, bounded by
/// `restored_slots`. `None` when the ordinal falls outside that range (a
/// zeroed key, or one past the persisted session's last workspace); the record
/// is then dropped rather than re-homed onto another workspace, because a
/// restore may not guess which workspace a record meant.
fn restored_slot_of(persisted_workspace: u64, restored_slots: usize) -> Option<usize> {
    let slot = usize::try_from(persisted_workspace).ok()?.checked_sub(1)?;
    (slot < restored_slots).then_some(slot)
}

/// First-binds a freshly allocated container: no predecessor, no ceiling, and
/// the driver's whole resolution as the grant set.
fn bind_restored_panel(
    host: &mut PanelRuntime,
    handle: PanelHandle,
    workspace: WorkspaceId,
    owner: &PluginId,
    resolution: &BTreeSet<String>,
    consented: &BTreeSet<String>,
) -> PanelRestoreOutcome {
    // A controller generation is a different value from the allocation epoch by
    // construction (`rebind_panel` refuses the epoch), so a first bind takes the
    // epoch's successor.
    let controller = match handle.generation.next() {
        Ok(generation) => generation,
        Err(_) => {
            return PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::RegistryRefused(PanelError::GenerationExhausted {
                    current: handle.generation,
                }),
            };
        }
    };
    match host.rebind_panel(handle.id, controller, owner, resolution, consented) {
        Ok(committed) => PanelRestoreOutcome::Bound {
            panel: handle.id,
            workspace,
            grants: committed.grants().clone(),
        },
        Err(error) => PanelRestoreOutcome::Dropped {
            reason: PanelRestoreDrop::RegistryRefused(error),
        },
    }
}

/// Presents the Decision E placeholder a driver answer names on a freshly
/// allocated, never-bound container.
fn present_restored_placeholder(
    host: &mut PanelRuntime,
    handle: PanelHandle,
    workspace: WorkspaceId,
    token: &'static str,
) -> PanelRestoreOutcome {
    match host.present_placeholder(handle.id, handle.generation, token) {
        Ok(presented) => match presented.placeholder {
            Some(placeholder) => PanelRestoreOutcome::Placeholder {
                panel: handle.id,
                workspace,
                reason: placeholder.reason(),
            },
            // Unreachable: every token used here selects `Placeholder` or
            // `Unavailable`, and both write the record's placeholder. A `Drop`
            // token (a user close) is never one of them.
            None => PanelRestoreOutcome::Dropped {
                reason: PanelRestoreDrop::RegistryRefused(PanelError::InvalidState {
                    current: PanelState::Created,
                    expected: "a placeholder outcome",
                }),
            },
        },
        Err(error) => PanelRestoreOutcome::Dropped {
            reason: PanelRestoreDrop::RegistryRefused(error),
        },
    }
}
