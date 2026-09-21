//! Host-owned lifecycle enforcement: degradation ladder, enforcement records,
//! and generation reload ordering (Isolation Resource RFC FS-2/FS-4/FS-6).
//!
//! This module is pure data plus bounded counters: no VM coupling, no file
//! I/O, no window/GPU coupling, and no `unsafe`. It is headlessly testable on
//! both Linux CI and the `windows-latest` job.
//!
//! - FS-2 degradation ladder: `refuse -> terminate callback -> suspend
//!   generation -> disable plugin`. Escalation timestamps per owner are kept
//!   in a sliding window ([`ESCALATION_WINDOW_SECS`]); three escalations
//!   inside the window suspend the generation. A suspended or disabled
//!   generation never resumes by itself: reactivation requires an explicit
//!   [`LifecycleEnforcer::reactivate`] call (the user action).
//! - FS-4 structured records: every enforcement action emits an
//!   [`EnforcementRecord`] carrying owner id, generation, budget dimension,
//!   observed value, limit, and action.
//! - FS-6 reload ordering: [`reload_generation`] disposes generation `N`
//!   before generation `N+1` activates. A failed activation restores `N`;
//!   when restoration also fails the plugin is disabled cleanly.
//!
//! Time is supplied by the caller as whole seconds since an arbitrary stable
//! epoch (`now_secs`). [`ManualClock`] provides a deterministic fake clock
//! for tests; [`SystemClock`] reads the wall clock for production use.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::PluginError;
use crate::registry::Generation;

/// Sliding window in which escalations accumulate toward suspension (seconds).
pub const ESCALATION_WINDOW_SECS: u64 = 60;

/// Escalations inside the window that suspend the generation.
pub const ESCALATIONS_TO_SUSPEND: usize = 3;

/// Upper bound on retained enforcement records; oldest are dropped first.
pub const MAX_ENFORCEMENT_RECORDS: usize = 256;

/// Upper bound on distinct tracked owners; new owners past this fail closed.
pub const MAX_TRACKED_OWNERS: usize = 1024;

/// Budget dimension an enforcement action was taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetDimension {
    /// Instruction budget (`RC-1` instruction ceiling).
    Instructions,
    /// Wall-clock budget (`RC-1` deadline, milliseconds).
    WallClockMs,
    /// Resident memory ceiling (`RC-2`, bytes).
    MemoryBytes,
    /// Queued event count (per-plugin or global queue bound).
    QueuedEvents,
    /// Queued payload bytes (per-plugin or global queue bound).
    QueuedBytes,
    /// Open file descriptor count (`RC-6`).
    FileDescriptors,
}

impl BudgetDimension {
    /// Stable identifier for diagnostics and `bitty plugin doctor` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::WallClockMs => "wall_clock_ms",
            Self::MemoryBytes => "memory_bytes",
            Self::QueuedEvents => "queued_events",
            Self::QueuedBytes => "queued_bytes",
            Self::FileDescriptors => "file_descriptors",
        }
    }
}

impl std::fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One rung of the FS-2 degradation ladder, in escalation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EnforcementAction {
    /// Deny the offending call; the generation keeps running.
    Refuse,
    /// Terminate the offending callback; the generation keeps running.
    TerminateCallback,
    /// Detach handlers for the generation; explicit reactivation required.
    SuspendGeneration,
    /// Detach handlers and revoke the plugin; explicit reactivation required.
    DisablePlugin,
}

impl EnforcementAction {
    /// Stable identifier for diagnostics and `bitty plugin doctor` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refuse => "refuse",
            Self::TerminateCallback => "terminate_callback",
            Self::SuspendGeneration => "suspend_generation",
            Self::DisablePlugin => "disable_plugin",
        }
    }

    /// True for rungs that detach the generation (`suspend` and `disable`).
    #[must_use]
    pub const fn detaches_generation(self) -> bool {
        matches!(self, Self::SuspendGeneration | Self::DisablePlugin)
    }
}

impl std::fmt::Display for EnforcementAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FS-4 structured enforcement record: one row per enforcement action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnforcementRecord {
    /// Owning plugin id (`owner.name` qualified identity).
    pub owner: String,
    /// Generation the action applied to.
    pub generation: Generation,
    /// Budget dimension that was exceeded.
    pub dimension: BudgetDimension,
    /// Observed value that exceeded the limit.
    pub observed: u64,
    /// Configured limit for the dimension.
    pub limit: u64,
    /// Ladder rung applied.
    pub action: EnforcementAction,
    /// Caller-supplied timestamp (seconds) of the enforcement.
    pub at_secs: u64,
}

/// Lifecycle status of one tracked owner as seen by the enforcer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginLifecycleStatus {
    /// Running normally.
    Active,
    /// Handlers detached after suspension; needs explicit reactivation.
    Suspended,
    /// Revoked after disable; needs explicit reactivation.
    Disabled,
}

impl PluginLifecycleStatus {
    /// Stable identifier for diagnostics output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Disabled => "disabled",
        }
    }
}

impl std::fmt::Display for PluginLifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Source of whole-second timestamps for [`LifecycleEnforcer`].
pub trait Clock {
    /// Current time in whole seconds since the clock epoch.
    fn now_secs(&self) -> u64;
}

/// Wall-clock [`Clock`] backed by [`SystemTime`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// Deterministic fake [`Clock`] for tests.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    now: Cell<u64>,
}

impl ManualClock {
    /// Create a clock fixed at `start_secs`.
    #[must_use]
    pub const fn new(start_secs: u64) -> Self {
        Self {
            now: Cell::new(start_secs),
        }
    }

    /// Move the clock forward by `delta_secs` (saturating).
    pub fn advance(&self, delta_secs: u64) {
        self.now.set(self.now.get().saturating_add(delta_secs));
    }

    /// Set the clock to an absolute value.
    pub fn set(&self, value_secs: u64) {
        self.now.set(value_secs);
    }
}

impl Clock for ManualClock {
    fn now_secs(&self) -> u64 {
        self.now.get()
    }
}

/// Host-owned FS-2/FS-4 enforcement state.
///
/// Tracks per-owner escalation timestamps in a sliding window, the lifecycle
/// status of each owner, and the bounded ledger of structured records.
/// Generations and statuses held here mirror the authoritative
/// [`crate::registry::Registry`]; this enforcer never activates or disposes
/// resources itself.
#[derive(Debug, Default)]
pub struct LifecycleEnforcer {
    escalations: BTreeMap<String, VecDeque<u64>>,
    statuses: BTreeMap<String, PluginLifecycleStatus>,
    generations: BTreeMap<String, Generation>,
    ledger: VecDeque<EnforcementRecord>,
    dropped_records: u64,
}

impl LifecycleEnforcer {
    /// Create an empty enforcer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current status of `owner` (`Active` for never-seen owners).
    #[must_use]
    pub fn status(&self, owner: &str) -> PluginLifecycleStatus {
        self.statuses
            .get(owner)
            .copied()
            .unwrap_or(PluginLifecycleStatus::Active)
    }

    /// Last generation reported for `owner`, if any.
    #[must_use]
    pub fn generation(&self, owner: &str) -> Option<Generation> {
        self.generations.get(owner).copied()
    }

    /// Escalation timestamps inside the window for `owner` (oldest first).
    #[must_use]
    pub fn escalations_in_window(&self, owner: &str) -> Vec<u64> {
        self.escalations
            .get(owner)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default()
    }

    /// All retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<EnforcementRecord> {
        self.ledger.iter().cloned().collect()
    }

    /// Retained records for `owner`, oldest first.
    #[must_use]
    pub fn records_for(&self, owner: &str) -> Vec<EnforcementRecord> {
        self.ledger
            .iter()
            .filter(|r| r.owner == owner)
            .cloned()
            .collect()
    }

    /// Records dropped from the bounded ledger due to overflow.
    #[must_use]
    pub const fn dropped_records(&self) -> u64 {
        self.dropped_records
    }

    /// Report a budget violation and apply one ladder rung (FS-2), emitting
    /// one structured record (FS-4).
    ///
    /// The rung follows the escalations for `owner` inside the sliding
    /// window ending at `now_secs`: first is [`EnforcementAction::Refuse`],
    /// second [`EnforcementAction::TerminateCallback`], third and later
    /// [`EnforcementAction::SuspendGeneration`]. A violation that arrives
    /// while the owner is already suspended escalates to
    /// [`EnforcementAction::DisablePlugin`]; a violation for a disabled
    /// owner records `DisablePlugin` without changing state.
    ///
    /// # Errors
    ///
    /// [`PluginError::LimitExceeded`] when `owner` is new and the owner
    /// table is already at [`MAX_TRACKED_OWNERS`].
    pub fn report_violation(
        &mut self,
        owner: &str,
        generation: Generation,
        dimension: BudgetDimension,
        observed: u64,
        limit: u64,
        now_secs: u64,
    ) -> Result<EnforcementRecord, PluginError> {
        if !self.statuses.contains_key(owner) && self.statuses.len() >= MAX_TRACKED_OWNERS {
            return Err(PluginError::LimitExceeded {
                field: "lifecycle.tracked_owners".to_string(),
                limit: MAX_TRACKED_OWNERS,
                actual: self.statuses.len().saturating_add(1),
            });
        }
        let current = self.status(owner);
        let window_start = now_secs.saturating_sub(ESCALATION_WINDOW_SECS);
        let stamps = self.escalations.entry(owner.to_string()).or_default();
        while stamps.front().is_some_and(|t| *t < window_start) {
            stamps.pop_front();
        }
        let action = match current {
            PluginLifecycleStatus::Disabled => EnforcementAction::DisablePlugin,
            PluginLifecycleStatus::Suspended => {
                self.statuses
                    .insert(owner.to_string(), PluginLifecycleStatus::Disabled);
                EnforcementAction::DisablePlugin
            }
            PluginLifecycleStatus::Active => {
                let count = stamps.len().saturating_add(1);
                if count >= ESCALATIONS_TO_SUSPEND {
                    self.statuses
                        .insert(owner.to_string(), PluginLifecycleStatus::Suspended);
                    EnforcementAction::SuspendGeneration
                } else if count == 2 {
                    EnforcementAction::TerminateCallback
                } else {
                    EnforcementAction::Refuse
                }
            }
        };
        stamps.push_back(now_secs);
        self.statuses
            .entry(owner.to_string())
            .or_insert(PluginLifecycleStatus::Active);
        self.generations.insert(owner.to_string(), generation);
        let record = EnforcementRecord {
            owner: owner.to_string(),
            generation,
            dimension,
            observed,
            limit,
            action,
            at_secs: now_secs,
        };
        if self.ledger.len() >= MAX_ENFORCEMENT_RECORDS {
            self.ledger.pop_front();
            self.dropped_records = self.dropped_records.wrapping_add(1);
        }
        self.ledger.push_back(record.clone());
        Ok(record)
    }

    /// Explicit user action that reactivates a suspended or disabled owner
    /// (FS-2). Clears that owner's escalation window so the ladder restarts.
    /// There is no automatic path back to `Active`.
    ///
    /// # Errors
    ///
    /// [`PluginError::InvalidState`] when `owner` is already active;
    /// [`PluginError::NotFound`] when `owner` was never tracked.
    pub fn reactivate(&mut self, owner: &str) -> Result<(), PluginError> {
        let current = self
            .statuses
            .get(owner)
            .copied()
            .ok_or_else(|| PluginError::NotFound {
                id: owner.to_string(),
            })?;
        match current {
            PluginLifecycleStatus::Active => Err(PluginError::InvalidState {
                id: owner.to_string(),
                current: current.to_string(),
                expected: format!(
                    "{} or {}",
                    PluginLifecycleStatus::Suspended,
                    PluginLifecycleStatus::Disabled
                ),
            }),
            PluginLifecycleStatus::Suspended | PluginLifecycleStatus::Disabled => {
                self.statuses
                    .insert(owner.to_string(), PluginLifecycleStatus::Active);
                if let Some(stamps) = self.escalations.get_mut(owner) {
                    stamps.clear();
                }
                Ok(())
            }
        }
    }
}

/// Outcome of an FS-6 generation reload transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReloadOutcome {
    /// Generation `N` was disposed and `N+1` activated.
    Activated,
    /// Activation of `N+1` failed and generation `N` was restored.
    Restored,
    /// Restoration failed (or disposal failed) and the plugin was disabled.
    Disabled,
}

impl ReloadOutcome {
    /// Stable identifier for diagnostics output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activated => "activated",
            Self::Restored => "restored",
            Self::Disabled => "disabled",
        }
    }
}

impl std::fmt::Display for ReloadOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Report from [`reload_generation`]: the outcome plus the original failure
/// when the outcome is `Restored` or `Disabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport<E> {
    /// How the transaction settled.
    pub outcome: ReloadOutcome,
    /// The disposal or activation error that forced a non-`Activated`
    /// outcome, if any.
    pub error: Option<E>,
}

/// Resource handle driven by [`reload_generation`] (FS-6).
///
/// The transaction calls `dispose_generation(old)` strictly before
/// `activate_generation(new)` so generation `N` resources can never observe
/// or cancel `N+1` except through host-mediated handoff of persisted state.
pub trait ReloadResources {
    /// Failure type for every step.
    type Error;

    /// Release all resources owned by `generation`.
    fn dispose_generation(&mut self, generation: Generation) -> Result<(), Self::Error>;
    /// Bring `generation` live (reserve and activate its resources).
    fn activate_generation(&mut self, generation: Generation) -> Result<(), Self::Error>;
    /// Bring back the previously disposed `generation` after a failed reload.
    fn restore_generation(&mut self, generation: Generation) -> Result<(), Self::Error>;
    /// Detach handlers and revoke the plugin without activating anything.
    fn disable_plugin(&mut self) -> Result<(), Self::Error>;
}

/// Dispose generation `old` before activating generation `new` (FS-6).
///
/// - Activation success yields `Activated` with no error.
/// - Activation failure restores `old`: restoration success yields
///   `Restored` carrying the activation error.
/// - Restoration failure (or disposal failure) disables the plugin cleanly
///   and yields `Disabled`, still carrying the original error. A failing
///   `disable_plugin` step cannot change that outcome; the original error
///   is retained.
pub fn reload_generation<R: ReloadResources>(
    resources: &mut R,
    old: Generation,
    new: Generation,
) -> ReloadReport<R::Error> {
    if let Err(error) = resources.dispose_generation(old) {
        let _ = resources.disable_plugin();
        return ReloadReport {
            outcome: ReloadOutcome::Disabled,
            error: Some(error),
        };
    }
    match resources.activate_generation(new) {
        Ok(()) => ReloadReport {
            outcome: ReloadOutcome::Activated,
            error: None,
        },
        Err(error) => {
            if resources.restore_generation(old).is_ok() {
                ReloadReport {
                    outcome: ReloadOutcome::Restored,
                    error: Some(error),
                }
            } else {
                let _ = resources.disable_plugin();
                ReloadReport {
                    outcome: ReloadOutcome::Disabled,
                    error: Some(error),
                }
            }
        }
    }
}
