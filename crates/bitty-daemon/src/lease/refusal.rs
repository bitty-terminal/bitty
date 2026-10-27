//! Closed refusals for the attach/lease/control state machine.
//!
//! Every variant is a whole refusal: no lease was created or removed, no
//! counter moved, and no token was rotated. No variant carries a secret, a
//! caller-supplied string, or another session's details.
//!
//! The IPC slice (bitty#2607) maps these onto the contract's service codes.
//! [`crate::lease`] documents the mapping; the important distinctions are
//! [`LeaseRefusal::ControllerRequired`] (visible session, actor is not the
//! controller), [`LeaseRefusal::ControlHeld`] (a claim or vacant-token path
//! found another live controller), and [`LeaseRefusal::InvalidControlToken`]
//! (control is vacant, but the token is missing, stale, rotated, or retired).

use std::error::Error;
use std::fmt;

use crate::registry::{LifecycleState, RegistryRefusal, TerminalOutcomeError};

/// A refused lease, control, or termination operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseRefusal {
    /// No session with that id exists in this instance (unknown or collected).
    SessionNotFound,
    /// The session is terminal and therefore read-only (`Denied/NotRunning`).
    NotRunning {
        /// The immutable terminal state.
        state: LifecycleState,
    },
    /// A controller claim or a genuine state change arrived while stopping.
    SessionStopping,
    /// The effectful request named a generation the session is not in.
    StaleGeneration {
        /// Current generation.
        current_generation: u64,
        /// Current state revision.
        current_revision: u64,
    },
    /// The actor is an observer, or a lease that is not the current controller.
    ControllerRequired,
    /// Control is held by a live controller lease elsewhere.
    ControlHeld,
    /// Control is vacant, but the presented token is stale, rotated, missing,
    /// or permanently retired.
    InvalidControlToken,
    /// This request used the operator authority on a controller-only path.
    OperatorPathRequired,
    /// The lease does not exist, has expired, or was revoked.
    NotAttached,
    /// A transfer target is live but is not an observer lease.
    TargetNotObserver,
    /// The per-session observer cap is already reached (`D-09`).
    MaxAttaches {
        /// The enforced observer cap; the controller lease is additional.
        limit: usize,
    },
    /// A generation-0 stop is committed; the spawn path must not bind.
    CancelPending,
    /// A pre-bind cancel was completed without a committed cancel intent.
    CancelNotPending,
    /// A termination observation does not fit the session's lifecycle state.
    UnexpectedExit {
        /// Current lifecycle state.
        state: LifecycleState,
    },
    /// The terminal classification contradicts its reason.
    TerminalOutcome(TerminalOutcomeError),
    /// The state-revision counter would wrap; the state fails closed.
    RevisionOverflow,
    /// The generation counter would wrap; the state fails closed.
    GenerationOverflow,
    /// The operating-system CSPRNG was unavailable while minting a secret or id.
    EntropyUnavailable,
    /// Fresh random ids repeatedly collided with live leases.
    IdCollision,
    /// A configured lease bound exceeds the reviewed system maximum.
    InvalidLimits {
        /// Limit name.
        limit: &'static str,
        /// Requested value.
        value: u64,
        /// Compiled ceiling.
        maximum: u64,
    },
    /// The registry refused the underlying record operation.
    Registry(RegistryRefusal),
}

impl fmt::Display for LeaseRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionNotFound => formatter.write_str("session not found"),
            Self::NotRunning { state } => {
                write!(formatter, "session is terminal in state {state}")
            }
            Self::SessionStopping => formatter.write_str("session is stopping"),
            Self::StaleGeneration {
                current_generation,
                current_revision,
            } => write!(
                formatter,
                "stale generation: current generation {current_generation}, \
                 revision {current_revision}"
            ),
            Self::ControllerRequired => formatter.write_str("controller authority required"),
            Self::ControlHeld => formatter.write_str("control is held by another lease"),
            Self::InvalidControlToken => formatter.write_str("invalid control token"),
            Self::OperatorPathRequired => {
                formatter.write_str("operator path required for this request")
            }
            Self::NotAttached => formatter.write_str("lease is not attached"),
            Self::TargetNotObserver => formatter.write_str("transfer target is not an observer"),
            Self::MaxAttaches { limit } => {
                write!(formatter, "attach limit {limit} reached")
            }
            Self::CancelPending => {
                formatter.write_str("generation-0 cancel is committed; bind is refused")
            }
            Self::CancelNotPending => formatter.write_str("no pre-bind cancel is pending"),
            Self::UnexpectedExit { state } => {
                write!(
                    formatter,
                    "termination observation is invalid in state {state}"
                )
            }
            Self::TerminalOutcome(error) => write!(formatter, "terminal outcome: {error}"),
            Self::RevisionOverflow => formatter.write_str("state revision would wrap"),
            Self::GenerationOverflow => formatter.write_str("generation would wrap"),
            Self::EntropyUnavailable => formatter.write_str("operating system entropy unavailable"),
            Self::IdCollision => formatter.write_str("server-side id generation kept colliding"),
            Self::InvalidLimits {
                limit,
                value,
                maximum,
            } => write!(
                formatter,
                "limit {limit} is {value}, above compiled maximum {maximum}"
            ),
            Self::Registry(refusal) => write!(formatter, "registry refusal: {refusal}"),
        }
    }
}

impl Error for LeaseRefusal {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TerminalOutcome(error) => Some(error),
            Self::Registry(refusal) => Some(refusal),
            _ => None,
        }
    }
}

impl From<RegistryRefusal> for LeaseRefusal {
    fn from(refusal: RegistryRefusal) -> Self {
        match refusal {
            RegistryRefusal::NotFound => Self::SessionNotFound,
            RegistryRefusal::NotRunning { state } => Self::NotRunning { state },
            RegistryRefusal::RevisionOverflow => Self::RevisionOverflow,
            RegistryRefusal::GenerationOverflow => Self::GenerationOverflow,
            other => Self::Registry(other),
        }
    }
}

impl From<TerminalOutcomeError> for LeaseRefusal {
    fn from(error: TerminalOutcomeError) -> Self {
        Self::TerminalOutcome(error)
    }
}
