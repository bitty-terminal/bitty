//! Stable `E_*` bridge codes for VM budget failures.
//!
//! Every runtime typed error surfaced by this crate carries a stable `E_*`
//! code in the `budget` diagnostic class. [`RuntimeFault`] models the three
//! failure styles the host must handle identically across VM backends
//! (`OutOfMemory`, `FuelExhausted`, `HostOpCancelled`); [`from_suspend_reason`]
//! maps the current [`SuspendReason`](crate::SuspendReason) onto that model,
//! and [`bridge_error_for_vm_error`] lifts a [`VmError`](crate::VmError)
//! into the bridge error the host reports to Lua.
//!
//! Wall-clock timeout maps to [`E_TIMEOUT`], the same value the host bridge
//! already emits via [`BridgeError::timeout`](crate::host::BridgeError),
//! so budget timeouts and host-call timeouts share one code and one class.
//!
//! Messages are host-authored and bounded: they carry counters only, never
//! untrusted content.

use crate::host::BridgeError;
use crate::{SuspendReason, VmError};

/// Stable code for wall-clock and host-operation timeouts (budget class).
///
/// Identical to the code [`BridgeError::timeout`](crate::host::BridgeError)
/// emits, so VM timeouts and bridge timeouts are indistinguishable by code.
pub const E_TIMEOUT: &str = "E_TIMEOUT";

/// Stable code for instruction-budget (fuel) exhaustion (budget class).
pub const E_BUDGET_INSTRUCTIONS: &str = "E_BUDGET_INSTRUCTIONS";

/// Stable code for heap-ceiling (out-of-memory) violations (budget class).
pub const E_BUDGET_MEMORY: &str = "E_BUDGET_MEMORY";

/// Diagnostic class shared by every budget failure.
pub const BUDGET_CLASS: &str = "budget";

/// Every stable budget-class code.
pub const ALL_BUDGET_CODES: [&str; 3] = [E_TIMEOUT, E_BUDGET_INSTRUCTIONS, E_BUDGET_MEMORY];

/// Whether `code` is a known stable budget-class code.
#[must_use]
pub fn is_budget_code(code: &str) -> bool {
    ALL_BUDGET_CODES.contains(&code)
}

/// Host-side model of VM failure styles.
///
/// The current backend reports these as [`SuspendReason`](crate::SuspendReason);
/// the Phodopus backend reports them as `OutOfMemory`, fuel exhaustion, and
/// host-operation cancellation. This enum is the stable mapping point both
/// backends share: each variant carries one `E_*` code in the `budget`
/// class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimeFault {
    /// Heap ceiling exceeded.
    OutOfMemory {
        /// Bytes used.
        used: usize,
        /// Ceiling bytes.
        limit: usize,
    },
    /// Instruction budget exhausted.
    FuelExhausted {
        /// Instructions consumed.
        used: u64,
        /// Budget.
        budget: u64,
    },
    /// Wall-clock deadline exceeded or host operation cancelled.
    HostOpCancelled {
        /// Elapsed milliseconds.
        elapsed_ms: u64,
        /// Budget milliseconds.
        budget_ms: u64,
    },
}

impl RuntimeFault {
    /// Stable `E_*` code for this failure.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::OutOfMemory { .. } => E_BUDGET_MEMORY,
            Self::FuelExhausted { .. } => E_BUDGET_INSTRUCTIONS,
            Self::HostOpCancelled { .. } => E_TIMEOUT,
        }
    }

    /// Diagnostic class for this failure (always the budget class).
    #[must_use]
    pub fn class(self) -> &'static str {
        BUDGET_CLASS
    }

    /// Render this failure as a bridge error.
    ///
    /// The wall-clock case returns exactly
    /// [`BridgeError::timeout`](crate::host::BridgeError), so VM timeouts
    /// and bridge timeouts share code, class, and message.
    #[must_use]
    pub fn to_bridge_error(self) -> BridgeError {
        match self {
            Self::HostOpCancelled { .. } => BridgeError::timeout(),
            Self::OutOfMemory { used, limit } => BridgeError::new(
                BUDGET_CLASS,
                E_BUDGET_MEMORY,
                format!("plugin heap exceeded ({used}/{limit} bytes)"),
            ),
            Self::FuelExhausted { used, budget } => BridgeError::new(
                BUDGET_CLASS,
                E_BUDGET_INSTRUCTIONS,
                format!("instruction budget exhausted ({used}/{budget})"),
            ),
        }
    }

    /// Map a suspend reason onto the backend-independent fault model.
    #[must_use]
    pub fn from_suspend_reason(reason: &SuspendReason) -> Self {
        match reason {
            SuspendReason::MemoryExceeded { used, limit } => Self::OutOfMemory {
                used: *used,
                limit: *limit,
            },
            SuspendReason::InstructionBudgetExceeded { used, budget } => Self::FuelExhausted {
                used: *used,
                budget: *budget,
            },
            SuspendReason::WallClockExceeded {
                elapsed_ms,
                budget_ms,
            } => Self::HostOpCancelled {
                elapsed_ms: *elapsed_ms,
                budget_ms: *budget_ms,
            },
        }
    }
}

/// Map a suspend reason to its stable bridge error.
#[must_use]
pub fn bridge_error_for_suspend(reason: &SuspendReason) -> BridgeError {
    RuntimeFault::from_suspend_reason(reason).to_bridge_error()
}

/// Map a VM error to its stable bridge error, if it is budget-class.
///
/// Suspension maps to the fault code for its reason; load, runtime, and
/// budget-configuration errors are not VM budget failures and yield `None`.
#[must_use]
pub fn bridge_error_for_vm_error(err: &VmError) -> Option<BridgeError> {
    match err {
        VmError::Suspended { reason } => Some(bridge_error_for_suspend(reason)),
        VmError::Load(_) | VmError::Runtime(_) | VmError::Budget(_) => None,
    }
}
