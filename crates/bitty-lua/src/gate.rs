//! Fail-closed load gate for plugin VMs (FS-7) and safe-mode policy (FS-8).
//!
//! A plugin VM must never be built without explicit RC-1/RC-2 budgets. The
//! public plugin constructors in this module ([`PluginVmBuilder`] and
//! [`build_plugin_vm`]) take budgets as a required input and refuse
//! fail-closed with [`VmError::Budget`](crate::VmError) when budgets are
//! missing or any enforced dimension is zero. There is no unguarded public
//! constructor here: the only way to obtain a [`LuaVm`](crate::LuaVm)
//! through this module is to supply valid budgets first.
//!
//! [`LoadPolicy`] additionally models `--safe`: under the safe policy no
//! third-party candidate is admitted, so hostile third-party sources stay
//! inert (never loaded, never executed). Selection operates on identities
//! only and touches no filesystem paths.

use crate::{
    LuaVm, RC1_INSTRUCTION_BUDGET, RC1_WALL_CLOCK_BUDGET_MS, RC1_WARNING_MS,
    RC2_MEMORY_PER_PLUGIN_BYTES, VmError,
};

/// RC-1/RC-2 budgets required to build a plugin VM.
///
/// `instruction_budget` and `wall_budget_ms` are the RC-1 per-VM instruction
/// and wall-clock ceilings; `memory_limit` is the RC-2 per-VM accounted-heap
/// ceiling. `warning_ms` is advisory only (sets the warning flag, never
/// suspends) and is not part of the fail-closed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmBudgets {
    /// RC-1 instruction budget per VM.
    pub instruction_budget: u64,
    /// RC-1 wall-clock budget per VM in milliseconds.
    pub wall_budget_ms: u64,
    /// RC-1 warning threshold in milliseconds (advisory).
    pub warning_ms: u64,
    /// RC-2 per-VM memory ceiling in bytes.
    pub memory_limit: usize,
}

impl VmBudgets {
    /// Fail-closed validation: every enforced dimension must be non-zero.
    ///
    /// # Errors
    ///
    /// [`VmError::Budget`](crate::VmError) when any enforced dimension is
    /// zero. The VM is never constructed on this path.
    pub fn validate(&self) -> Result<(), VmError> {
        if self.instruction_budget == 0 {
            return Err(VmError::Budget(
                "instruction budget (RC-1) must be greater than zero".to_string(),
            ));
        }
        if self.wall_budget_ms == 0 {
            return Err(VmError::Budget(
                "wall-clock budget (RC-1) must be greater than zero".to_string(),
            ));
        }
        if self.memory_limit == 0 {
            return Err(VmError::Budget(
                "memory ceiling (RC-2) must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for VmBudgets {
    /// Default RC budgets, identical to [`LuaVm::new`].
    fn default() -> Self {
        Self {
            instruction_budget: RC1_INSTRUCTION_BUDGET,
            wall_budget_ms: RC1_WALL_CLOCK_BUDGET_MS,
            warning_ms: RC1_WARNING_MS,
            memory_limit: RC2_MEMORY_PER_PLUGIN_BYTES,
        }
    }
}

/// Fail-closed builder for plugin VMs.
///
/// Budgets are mandatory: [`build`](PluginVmBuilder::build) refuses with
/// [`VmError::Budget`](crate::VmError) when [`budgets`](PluginVmBuilder::budgets)
/// was never called or any enforced dimension is zero.
pub struct PluginVmBuilder {
    id: String,
    budgets: Option<VmBudgets>,
}

impl PluginVmBuilder {
    /// Start building a plugin VM for `id` with no budgets configured.
    ///
    /// The builder refuses to [`build`](PluginVmBuilder::build) until
    /// [`budgets`](PluginVmBuilder::budgets) supplies valid budgets.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            budgets: None,
        }
    }

    /// Supply the RC-1/RC-2 budgets for the VM.
    #[must_use]
    pub fn budgets(mut self, budgets: VmBudgets) -> Self {
        self.budgets = Some(budgets);
        self
    }

    /// Build the VM, refusing fail-closed without valid budgets.
    ///
    /// # Errors
    ///
    /// [`VmError::Budget`](crate::VmError) when budgets were never supplied
    /// or fail validation. No VM is constructed on this path.
    ///
    /// The `with_budgets` call below is the single legitimate internal use of
    /// the deprecated raw constructor: the gate is the only path that may
    /// invoke it, so the deprecation lint is silenced here and nowhere else.
    #[allow(deprecated)]
    pub fn build(self) -> Result<LuaVm, VmError> {
        let budgets = self.budgets.ok_or_else(|| {
            VmError::Budget(
                "plugin VM requires explicit RC-1/RC-2 budgets (fail-closed)".to_string(),
            )
        })?;
        budgets.validate()?;
        Ok(LuaVm::with_budgets(
            self.id,
            budgets.instruction_budget,
            budgets.wall_budget_ms,
            budgets.warning_ms,
            budgets.memory_limit,
        ))
    }
}

/// Build a plugin VM for `id`, refusing fail-closed without valid budgets.
///
/// This is the function form of [`PluginVmBuilder::build`]: `None` or
/// zero-dimension budgets yield [`VmError::Budget`](crate::VmError) and no
/// VM is constructed.
///
/// # Errors
///
/// [`VmError::Budget`](crate::VmError) when `budgets` is `None` or fails
/// validation.
pub fn build_plugin_vm(
    id: impl Into<String>,
    budgets: Option<VmBudgets>,
) -> Result<LuaVm, VmError> {
    let builder = PluginVmBuilder::new(id);
    match budgets {
        Some(budgets) => builder.budgets(budgets).build(),
        None => builder.build(),
    }
}

/// Load policy selecting which plugin candidates may load.
///
/// The safe policy (`--safe`) admits zero third-party plugins: selection
/// drops every third-party candidate before any VM is built, so hostile
/// third-party sources stay inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadPolicy {
    safe_mode: bool,
}

impl LoadPolicy {
    /// Standard policy: first-party and third-party candidates may load.
    #[must_use]
    pub fn standard() -> Self {
        Self { safe_mode: false }
    }

    /// Safe policy (`--safe`): only first-party candidates may load.
    #[must_use]
    pub fn safe_mode() -> Self {
        Self { safe_mode: true }
    }

    /// Whether this is the safe policy.
    #[must_use]
    pub fn is_safe_mode(&self) -> bool {
        self.safe_mode
    }

    /// Whether third-party candidates are admitted.
    #[must_use]
    pub fn allows_third_party(&self) -> bool {
        !self.safe_mode
    }
}

/// A plugin load candidate: identity plus origin.
///
/// First-party means shipped with the host; everything else is third-party
/// and untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCandidate {
    /// Plugin identity.
    pub id: String,
    /// Whether the candidate is third-party (untrusted).
    pub third_party: bool,
}

impl PluginCandidate {
    /// A first-party candidate for `id`.
    #[must_use]
    pub fn first_party(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            third_party: false,
        }
    }

    /// A third-party candidate for `id`.
    #[must_use]
    pub fn third_party(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            third_party: true,
        }
    }
}

/// Select the candidates the policy admits, preserving order.
///
/// Under the safe policy every third-party candidate is dropped here, before
/// any VM exists, so hostile sources are never loaded and never executed.
#[must_use]
pub fn select_candidates(
    policy: &LoadPolicy,
    candidates: &[PluginCandidate],
) -> Vec<PluginCandidate> {
    candidates
        .iter()
        .filter(|candidate| !candidate.third_party || policy.allows_third_party())
        .cloned()
        .collect()
}

/// Count admitted third-party candidates (always zero under safe mode).
#[must_use]
pub fn count_third_party_selected(policy: &LoadPolicy, candidates: &[PluginCandidate]) -> usize {
    select_candidates(policy, candidates)
        .iter()
        .filter(|candidate| candidate.third_party)
        .count()
}
