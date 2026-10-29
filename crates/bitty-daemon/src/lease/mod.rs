//! Attach leases, the control token, control transfer, and the abort/kill
//! state machine (bitty#2609, contract `D-03`, `D-04`, `D-05`, `D-07`, `D-09`,
//! `D-17`, `D-18`).
//!
//! [`SessionHost`] wraps the in-memory [`SessionRegistry`](crate::registry::SessionRegistry)
//! with the per-session lease table, the control relation, and the stop
//! pipeline. One `&mut SessionHost` makes every operation atomic: a refusal
//! leaves the registry, the lease table, and the token untouched, except for the
//! one documented case where a claim or transfer commits an already-expired
//! controller's due detach before a later counter fault refuses that operation.
//!
//! # What this module owns
//!
//! - One controller lease per session and observers up to
//!   `max_attaches_per_session`, with the controller additional to that cap
//!   (`D-09`). Every attach gets a distinct server-minted 128-bit [`AttachId`].
//! - Observer and controller roles. An observer never acquires control by
//!   waiting, by a controller disconnect, by racing an attach, or by presenting
//!   the current token. Control moves only by an explicit
//!   [`SessionHost::claim_controller`] over a vacant control relation or by
//!   [`SessionHost::transfer_control`] to a live observer.
//! - The 256-bit control token: minted from the OS CSPRNG at create, returned
//!   once, stored only as a domain-separated SHA-256 digest, verified in
//!   constant time, rotated on claim and transfer, and retired on controller
//!   revocation and terminal transition. A claim returns the rotated token on
//!   the claiming connection (that connection becomes the controller); a
//!   transfer returns it only inside [`TransferDelivery`], addressed to the
//!   **target** lease, so the requesting connection never receives the new
//!   controller's secret.
//! - Lease expiry, renewal, explicit detach, and revocation on the injected
//!   [`MonotonicMs`](crate::registry::MonotonicMs). Expiry only detaches; it
//!   never aborts and never changes the generation. [`SessionHost::lease`] is
//!   a lookup that also returns an expired record still awaiting the sweep;
//!   [`SessionHost::authorize_content_read`] and
//!   [`SessionHost::authorize_controller`] are the liveness-checked entry
//!   points. [`SessionHost::attach_observer`] sweeps the session at entry so
//!   expired records stay inside the record bound and their detaches reach the
//!   event layer, and [`SessionHost::snapshot`] composes the live
//!   `attach_count` (`R-S18`).
//! - Controller-only content authority: [`SessionHost::authorize_controller`]
//!   gates input/resize, and [`SessionHost::record_resize`] increments the
//!   revision exactly once. An observer resize is refused and changes nothing.
//! - The stop pipeline: generation-0 pre-bind cancel intent, `stopping` with a
//!   graceful deadline, escalation to kill, terminal classification
//!   (`worker_exit`, `worker_signal`, `spawn_failed`, `supervision_failed`, or
//!   the accepted stop reason), and exactly one terminal notice.
//!
//! # Authorization order and layer ownership
//!
//! Contract order is **authenticate, role/scope, generation, token or
//! approval**. This module starts at the role step; everything before it and
//! the approval store belong to the IPC slice.
//!
//! | Check | Owner | Notes |
//! | --- | --- | --- |
//! | Peer credentials, per-message credential re-check, peer process re-check | bitty#2607 | Before any request is parsed. |
//! | Instance match | bitty#2607 | Before session lookup; `StaleInstance`. |
//! | Scope (`session.inspect`, `session.attach`, `session.control`, `daemon.*`) | bitty#2607 | Deny-by-default; `ScopeViolation` before lookup. |
//! | Visibility (live lease, token, or grant; uniform `NotFound`) | bitty#2607 | This module must not be reachable for an invisible session. |
//! | Observer grant: mint, single-use consume, TTL, peer binding, revoke | bitty#2609 follow-up (grant store), consumed by bitty#2607 | [`SessionHost::attach_observer`] assumes a validated grant; bitty#2607 consumes it only after that call returns `Ok`, so a refused attach does not burn the grant. Never expose `attach_observer` to a bare connection. |
//! | Operator capability | bitty#2607 | Gates the `operator` authority and the operator-only methods. [`ControlAuthority::operator`] is unauthenticated by construction: only the daemon's own service code may build it, and it must never be derived from wire bytes without the `daemon.control` capability check. |
//! | Lease live/expired, role, controller relation | bitty#2609 (this module) | `ControllerRequired`, `ControlHeld`, `NotAttached`. A lease-bearing authority is role-decided: an observer is refused even with a valid token. |
//! | Generation fence | bitty#2609 (this module) | `StaleGeneration` before any token comparison. |
//! | Control-token digest comparison | bitty#2609 (this module) | Constant time; `InvalidControlToken`. The token is consulted only for an authority with no lease. |
//! | Transfer token delivery | bitty#2607 (router), bitty#2609 (value) | [`TransferDelivery`] yields the rotated secret only for the target attach id; the request response never carries it (`D-09`). |
//! | PTY containment, graceful/kill signals, reap, cleanup class | bitty#2791 | This module emits the deadline and accepts the verified [`CleanupClass`](crate::registry::CleanupClass). |
//! | Event envelope, per-lease queues, terminal slot reservation, `TerminalEventLost` | bitty#2792 | This module returns the one terminal [`TerminalNotice`] and the leases it covers. |
//! | Idempotency key stores (session-scoped and create-key) | bitty#2603 | Termination and revocation are keyless and never consult a store. |
//! | Audit record emission | bitty#2607, bitty#2608 | [`Actor`] and [`TerminalNotice`] carry attribution, never a token. |
//!
//! # Service-code mapping for bitty#2607
//!
//! | [`LeaseRefusal`] | Contract result |
//! | --- | --- |
//! | `SessionNotFound` | `NotFound` |
//! | `NotRunning` | `Denied/NotRunning` (or the terminal snapshot for an idempotent repeat) |
//! | `SessionStopping` | `Denied/SessionStopping` |
//! | `StaleGeneration` | `Denied/StaleGeneration` with generation and revision |
//! | `ControllerRequired` | `Denied/ControllerRequired` |
//! | `ControlHeld` | `Denied/ControlHeld` |
//! | `InvalidControlToken` | `Denied/InvalidControlToken` |
//! | `NotAttached` | `NotFound` for a foreign handle; duplicate detach of a handle never issued in this session is a successful [`Detached`] with `was_attached = false` |
//! | `TargetNotObserver` | `InvalidRequest` (the transfer target must be a live observer) |
//! | `MaxAttaches` | `LimitExceeded` with the advertised cap |
//! | `CancelPending` | Internal: the spawn path must not bind |
//! | `CancelNotPending` | Internal: no generation-0 cancel was committed |
//! | `UnexpectedExit` | `Internal`; fail closed |
//! | `TerminalOutcome` | `Internal`; the classification contradicts the reason |
//! | `RevisionOverflow`, `GenerationOverflow`, `EntropyUnavailable`, `IdCollision` | `Internal`; fail closed |
//! | `InvalidLimits` | Startup failure (exit `1`); never a session outcome |
//! | `OperatorPathRequired` | `Internal`; the caller used the operator authority on a controller-only path |
//! | `Registry(inner)` | Delegate to the registry mapping in bitty#2607 |
//!
//! # Deliberately not in this slice
//!
//! The observer-grant store, the idempotency stores, event envelopes and
//! queues, PTY containment and signalling, peer authentication, scopes, audit
//! files, and endpoint handling are owned by the slices named above. This
//! module starts no listener, opens no file, spawns no process, and never
//! sleeps.

mod host;
mod record;
mod refusal;
mod token;

pub use self::host::{
    AbortEscalation, AbortStage, Actor, Attached, Claimed, ConsumerStopCause, ControlAuthority,
    ControlTokenState, ControlView, CreatedSession, DaemonStopCause, Detached, ExitObservation,
    LeaseExpiry, LeaseHeartbeat, PreBindCancelAccepted, SessionHost, StopAccepted, StopOutcome,
    StopView, TerminalNotice, Termination, TransferDelivery, Transferred,
};
pub use self::record::{
    ABORT_GRACE_MS_DEFAULT, ABORT_GRACE_MS_SYSTEM_MAX, ATTACH_LEASE_TTL_MS_DEFAULT,
    ATTACH_LEASE_TTL_MS_SYSTEM_MAX, DRAIN_GRACE_MS_DEFAULT, DRAIN_GRACE_MS_SYSTEM_MAX, Lease,
    LeaseLimits, LeaseRole, MAX_ATTACHES_PER_SESSION_DEFAULT, MAX_ATTACHES_PER_SESSION_SYSTEM_MAX,
};
pub use self::refusal::LeaseRefusal;
pub use self::token::ControlToken;

pub use crate::registry::AttachId;
