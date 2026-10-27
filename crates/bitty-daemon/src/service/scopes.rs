//! The `bittyd`-owned service scope registry (`D-07`, "Scope families").
//!
//! The `bitty-ipc` v1 families (`terminal.*`, `view.*`, `config.*`,
//! `plugin.*`, `process.spawn`, `debug.*`, `ai.*`) stay owned by `bitty-ipc`
//! and are never re-meant here. The service adds one closed, deny-by-default
//! family pair — `session.*` and `daemon.*` — and this module is its single
//! spelling source:
//!
//! | Scope             | Baseline for a consumer | Grants                                                                                     |
//! | ----------------- | ----------------------- | ------------------------------------------------------------------------------------------ |
//! | `session.inspect` | yes                     | Read metadata status and list sessions with a live lease on this connection                |
//! | `session.host`    | no                      | Create a session of an enabled kind; requires per-client consent and `process.spawn`       |
//! | `session.attach`  | yes                     | Accept a valid observer grant to obtain an observer lease                                  |
//! | `session.control` | yes                     | With the current control token or controller lease: claim, grant, abort, transfer, revoke  |
//! | `daemon.inspect`  | no                      | Read daemon status, orphan counts, and the operator all-session view                       |
//! | `daemon.control`  | no                      | Stop, drain, force, revoke any lease, mint host/observer grants, verify orphans            |
//!
//! Rules encoded here (`D-07`, bitty#2607 mechanism decision):
//!
//! - no scope implies another, and `session.control` is necessary but never
//!   sufficient for a control action;
//! - `session.host` is never baseline: only a consumed host-consent grant adds
//!   it (`daemon.negotiate`);
//! - the operator capability adds `daemon.inspect` and `daemon.control`;
//! - `terminal.*` is never stretched to cover session or daemon
//!   administration, so the one method whose contract scope is a v1 scope
//!   (`session.resize`, `terminal.manage`) is expressed as a transport
//!   requirement instead of a service scope;
//! - a well-formed method name outside the service catalog is `NotFound`
//!   (deny-by-default), and a name that violates the `bitty-ipc` method grammar
//!   is `InvalidMethod`. The grammar and the v1 registry are reused from
//!   `bitty-ipc-api`, never re-implemented.
//!
//! The physical home of this table is a mechanism choice: it lives here rather
//! than in the `bitty-ipc` `Scope` enum, which stays closed for v1. The method
//! grammar, name validation, and the v1 method registry still come from
//! `bitty-ipc-api`, so there is exactly one grammar and one v1 vocabulary.

use std::fmt;

use bitty_ipc_api::scope::{Scope as TransportScope, validate_method_name};

use crate::peer::DispatchBoundary;

/// One `bittyd` service scope (`D-07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceScope {
    /// `session.inspect`.
    SessionInspect,
    /// `session.host`.
    SessionHost,
    /// `session.attach`.
    SessionAttach,
    /// `session.control`.
    SessionControl,
    /// `daemon.inspect`.
    DaemonInspect,
    /// `daemon.control`.
    DaemonControl,
}

impl ServiceScope {
    /// Every service scope in presentation order.
    pub const ALL: [Self; 6] = [
        Self::SessionInspect,
        Self::SessionHost,
        Self::SessionAttach,
        Self::SessionControl,
        Self::DaemonInspect,
        Self::DaemonControl,
    ];

    /// The frozen wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionInspect => "session.inspect",
            Self::SessionHost => "session.host",
            Self::SessionAttach => "session.attach",
            Self::SessionControl => "session.control",
            Self::DaemonInspect => "daemon.inspect",
            Self::DaemonControl => "daemon.control",
        }
    }

    /// The family prefix, `session` or `daemon`.
    #[must_use]
    pub const fn family(self) -> &'static str {
        match self {
            Self::SessionInspect
            | Self::SessionHost
            | Self::SessionAttach
            | Self::SessionControl => "session",
            Self::DaemonInspect | Self::DaemonControl => "daemon",
        }
    }

    /// Whether the operator capability, not a consumer, grants this scope.
    #[must_use]
    pub const fn is_operator(self) -> bool {
        matches!(self, Self::DaemonInspect | Self::DaemonControl)
    }

    /// The scopes an ordinary consumer connection receives (`D-07`).
    ///
    /// `session.host` is deliberately absent; it is added only by consuming a
    /// host-consent grant at `daemon.negotiate`.
    #[must_use]
    pub const fn baseline() -> ServiceScopeSet {
        let mut set = ServiceScopeSet::EMPTY;
        set = set.with(Self::SessionInspect);
        set = set.with(Self::SessionAttach);
        set.with(Self::SessionControl)
    }

    /// The scopes the operator capability adds (`D-07`).
    #[must_use]
    pub const fn operator() -> ServiceScopeSet {
        let mut set = ServiceScopeSet::EMPTY;
        set = set.with(Self::DaemonInspect);
        set.with(Self::DaemonControl)
    }
}

impl fmt::Display for ServiceScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded set of service scopes.
///
/// There are six scopes, so the set is a `u8` bitmask: no allocation, no
/// growth past the closed family, and a deterministic iteration order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceScopeSet(u8);

impl ServiceScopeSet {
    /// The empty, deny-by-default set.
    pub const EMPTY: Self = Self(0);

    const fn bit(scope: ServiceScope) -> u8 {
        1 << (scope as u8)
    }

    /// A set holding every service scope.
    #[must_use]
    pub const fn all() -> Self {
        let mut set = Self::EMPTY;
        let mut index = 0;
        while index < ServiceScope::ALL.len() {
            set = set.with(ServiceScope::ALL[index]);
            index += 1;
        }
        set
    }

    /// The baseline consumer set.
    #[must_use]
    pub const fn baseline() -> Self {
        ServiceScope::baseline()
    }

    /// The operator set.
    #[must_use]
    pub const fn operator() -> Self {
        ServiceScope::operator()
    }

    /// This set plus `scope`.
    #[must_use]
    pub const fn with(mut self, scope: ServiceScope) -> Self {
        self.0 |= Self::bit(scope);
        self
    }

    /// Insert `scope` in place.
    pub fn insert(&mut self, scope: ServiceScope) {
        self.0 |= Self::bit(scope);
    }

    /// Remove `scope` in place.
    pub fn remove(&mut self, scope: ServiceScope) {
        self.0 &= !Self::bit(scope);
    }

    /// Whether `scope` is present.
    #[must_use]
    pub const fn contains(self, scope: ServiceScope) -> bool {
        self.0 & Self::bit(scope) != 0
    }

    /// Whether every scope in `other` is present.
    #[must_use]
    pub const fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Number of held scopes.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The held scopes in presentation order.
    pub fn iter(self) -> impl Iterator<Item = ServiceScope> {
        ServiceScope::ALL
            .into_iter()
            .filter(move |scope| self.contains(*scope))
    }

    /// The held scope names in presentation order.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        self.iter().map(ServiceScope::as_str)
    }

    /// Union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl fmt::Display for ServiceScopeSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for name in self.names() {
            if !first {
                formatter.write_str(" ")?;
            }
            first = false;
            formatter.write_str(name)?;
        }
        Ok(())
    }
}

/// The contract's idempotency class for one method (`D-18`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyClass {
    /// Keyless and naturally idempotent; never consults a store.
    Keyless,
    /// Carries a client-generated 128-bit key and consults a bounded store.
    Keyed,
    /// No key and no frozen duplicate rule (`session.resize`,
    /// `daemon.host_grant` mint); the method's own contract text decides.
    UnkeyedOpen,
}

impl IdempotencyClass {
    /// Static spelling for tests and diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keyless => "keyless",
            Self::Keyed => "keyed",
            Self::UnkeyedOpen => "unkeyed",
        }
    }
}

/// The scope requirement a resolved method imposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeRequirement {
    /// No service scope: `daemon.negotiate` is pre-authentication.
    None,
    /// Any one of the listed service scopes satisfies the requirement.
    Service(&'static [ServiceScope]),
    /// A `bitty-ipc` v1 transport scope, unchanged and never re-meant.
    Transport(&'static [TransportScope]),
}

/// One callable method in the frozen service catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ServiceMethod {
    /// `daemon.negotiate`.
    DaemonNegotiate,
    /// `daemon.status`.
    DaemonStatus,
    /// `daemon.stop`.
    DaemonStop,
    /// `daemon.host_grant`.
    DaemonHostGrant,
    /// `daemon.orphans.verify`.
    DaemonOrphansVerify,
    /// `session.create`.
    SessionCreate,
    /// `session.status`.
    SessionStatus,
    /// `session.list`.
    SessionList,
    /// `session.attach`.
    SessionAttach,
    /// `session.grant`.
    SessionGrant,
    /// `session.grant.revoke`.
    SessionGrantRevoke,
    /// `session.detach`.
    SessionDetach,
    /// `session.control.transfer`.
    SessionControlTransfer,
    /// `session.resize`.
    SessionResize,
    /// `session.abort`.
    SessionAbort,
    /// `session.kill`.
    SessionKill,
    /// `lease.heartbeat`.
    LeaseHeartbeat,
}

const SESSION_CONTROL: [ServiceScope; 1] = [ServiceScope::SessionControl];
const SESSION_CONTROL_OR_OPERATOR: [ServiceScope; 2] =
    [ServiceScope::SessionControl, ServiceScope::DaemonControl];
const DAEMON_INSPECT: [ServiceScope; 1] = [ServiceScope::DaemonInspect];
const DAEMON_CONTROL: [ServiceScope; 1] = [ServiceScope::DaemonControl];
const SESSION_INSPECT: [ServiceScope; 1] = [ServiceScope::SessionInspect];
const SESSION_HOST: [ServiceScope; 1] = [ServiceScope::SessionHost];
const SESSION_ATTACH: [ServiceScope; 1] = [ServiceScope::SessionAttach];
const TERMINAL_MANAGE: [TransportScope; 1] = [TransportScope::TerminalManage];

impl ServiceMethod {
    /// Every method in the frozen catalog order.
    pub const ALL: [Self; 17] = [
        Self::DaemonNegotiate,
        Self::DaemonStatus,
        Self::DaemonStop,
        Self::DaemonHostGrant,
        Self::DaemonOrphansVerify,
        Self::SessionCreate,
        Self::SessionStatus,
        Self::SessionList,
        Self::SessionAttach,
        Self::SessionGrant,
        Self::SessionGrantRevoke,
        Self::SessionDetach,
        Self::SessionControlTransfer,
        Self::SessionResize,
        Self::SessionAbort,
        Self::SessionKill,
        Self::LeaseHeartbeat,
    ];

    /// The frozen wire spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DaemonNegotiate => "daemon.negotiate",
            Self::DaemonStatus => "daemon.status",
            Self::DaemonStop => "daemon.stop",
            Self::DaemonHostGrant => "daemon.host_grant",
            Self::DaemonOrphansVerify => "daemon.orphans.verify",
            Self::SessionCreate => "session.create",
            Self::SessionStatus => "session.status",
            Self::SessionList => "session.list",
            Self::SessionAttach => "session.attach",
            Self::SessionGrant => "session.grant",
            Self::SessionGrantRevoke => "session.grant.revoke",
            Self::SessionDetach => "session.detach",
            Self::SessionControlTransfer => "session.control.transfer",
            Self::SessionResize => "session.resize",
            Self::SessionAbort => "session.abort",
            Self::SessionKill => "session.kill",
            Self::LeaseHeartbeat => "lease.heartbeat",
        }
    }

    /// Resolve a frozen method name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|method| method.name() == name)
    }

    /// The scope requirement for this method.
    #[must_use]
    pub const fn scope(self) -> ScopeRequirement {
        match self {
            Self::DaemonNegotiate => ScopeRequirement::None,
            Self::DaemonStatus => ScopeRequirement::Service(&DAEMON_INSPECT),
            Self::DaemonStop
            | Self::DaemonHostGrant
            | Self::DaemonOrphansVerify
            | Self::SessionKill => ScopeRequirement::Service(&DAEMON_CONTROL),
            Self::SessionCreate => ScopeRequirement::Service(&SESSION_HOST),
            Self::SessionStatus | Self::SessionList => ScopeRequirement::Service(&SESSION_INSPECT),
            Self::SessionAttach => ScopeRequirement::Service(&SESSION_ATTACH),
            Self::SessionGrant | Self::SessionGrantRevoke => {
                ScopeRequirement::Service(&SESSION_CONTROL_OR_OPERATOR)
            }
            Self::SessionDetach => ScopeRequirement::Service(&SESSION_CONTROL_OR_OPERATOR),
            Self::SessionControlTransfer | Self::SessionAbort => {
                ScopeRequirement::Service(&SESSION_CONTROL)
            }
            Self::SessionResize => ScopeRequirement::Transport(&TERMINAL_MANAGE),
            Self::LeaseHeartbeat => ScopeRequirement::None,
        }
    }

    /// The alternative controller scope for `session.attach`.
    ///
    /// An observer attach requires `session.attach`; a controller claim is a
    /// `session.control` action. Every other method has one requirement.
    #[must_use]
    pub const fn controller_scope(self) -> Option<ServiceScope> {
        match self {
            Self::SessionAttach => Some(ServiceScope::SessionControl),
            _ => None,
        }
    }

    /// The peer-re-check boundary this method applies, if any.
    ///
    /// A mode-dependent method (`daemon.host_grant`, `session.detach`, and
    /// `session.abort`) can reach more than one boundary; [`Self::boundaries`]
    /// is the complete set and this value is its first entry.
    #[must_use]
    pub const fn boundary(self) -> Option<DispatchBoundary> {
        match self {
            Self::SessionCreate => Some(DispatchBoundary::CreateSession),
            Self::SessionAttach => Some(DispatchBoundary::ControllerAttach),
            Self::SessionAbort => Some(DispatchBoundary::Abort),
            Self::SessionKill => Some(DispatchBoundary::Kill),
            Self::SessionControlTransfer => Some(DispatchBoundary::ControlTransfer),
            Self::SessionDetach => Some(DispatchBoundary::DetachOther),
            Self::SessionGrant => Some(DispatchBoundary::ObserverGrantMint),
            Self::SessionGrantRevoke => Some(DispatchBoundary::ObserverGrantRevoke),
            Self::DaemonHostGrant => Some(DispatchBoundary::HostGrantMint),
            Self::DaemonStop => Some(DispatchBoundary::DaemonStop),
            Self::DaemonNegotiate
            | Self::DaemonStatus
            | Self::DaemonOrphansVerify
            | Self::SessionStatus
            | Self::SessionList
            | Self::SessionResize
            | Self::LeaseHeartbeat => None,
        }
    }

    /// Every peer-re-check boundary this method can reach.
    ///
    /// `daemon.host_grant` mints and revokes host consent, so both grant
    /// boundaries belong to it. Every other privileged method reaches exactly
    /// one boundary.
    #[must_use]
    pub const fn boundaries(self) -> &'static [DispatchBoundary] {
        match self {
            Self::SessionCreate => &[DispatchBoundary::CreateSession],
            Self::SessionAttach => &[DispatchBoundary::ControllerAttach],
            Self::SessionAbort => &[DispatchBoundary::Abort],
            Self::SessionKill => &[DispatchBoundary::Kill],
            Self::SessionControlTransfer => &[DispatchBoundary::ControlTransfer],
            Self::SessionDetach => &[DispatchBoundary::DetachOther],
            Self::SessionGrant => &[DispatchBoundary::ObserverGrantMint],
            Self::SessionGrantRevoke => &[DispatchBoundary::ObserverGrantRevoke],
            Self::DaemonHostGrant => &[
                DispatchBoundary::HostGrantMint,
                DispatchBoundary::HostGrantRevoke,
            ],
            Self::DaemonStop => &[DispatchBoundary::DaemonStop],
            Self::DaemonNegotiate
            | Self::DaemonStatus
            | Self::DaemonOrphansVerify
            | Self::SessionStatus
            | Self::SessionList
            | Self::SessionResize
            | Self::LeaseHeartbeat => &[],
        }
    }

    /// The capability that gates this method family, when one does.
    #[must_use]
    pub const fn capability(self) -> Option<&'static str> {
        match self {
            Self::SessionCreate => Some("terminal_sessions"),
            Self::SessionAttach | Self::SessionGrant | Self::SessionGrantRevoke => {
                Some("observer_grants")
            }
            Self::SessionControlTransfer => Some("control_transfer"),
            _ => None,
        }
    }

    /// The `D-18` idempotency class.
    #[must_use]
    pub const fn idempotency(self) -> IdempotencyClass {
        match self {
            Self::SessionCreate
            | Self::SessionAttach
            | Self::SessionGrant
            | Self::SessionControlTransfer => IdempotencyClass::Keyed,
            Self::SessionResize | Self::DaemonHostGrant => IdempotencyClass::UnkeyedOpen,
            _ => IdempotencyClass::Keyless,
        }
    }

    /// The audit action label for this method.
    ///
    /// Mode-dependent methods (`daemon.host_grant`) use the mint label; the
    /// revoke mode records its own more specific label inside the handler.
    #[must_use]
    pub const fn audit_action(self) -> crate::audit::AuditAction {
        use crate::audit::AuditAction;
        match self {
            Self::DaemonNegotiate => AuditAction::Negotiate,
            Self::DaemonStatus => AuditAction::DaemonStatus,
            Self::DaemonStop => AuditAction::Stop,
            Self::DaemonHostGrant => AuditAction::HostGrant,
            Self::DaemonOrphansVerify => AuditAction::OrphanVerify,
            Self::SessionCreate => AuditAction::Create,
            Self::SessionStatus => AuditAction::Status,
            Self::SessionList => AuditAction::List,
            Self::SessionAttach => AuditAction::Attach,
            Self::SessionGrant => AuditAction::Grant,
            Self::SessionGrantRevoke => AuditAction::GrantRevoke,
            Self::SessionDetach => AuditAction::Detach,
            Self::SessionControlTransfer => AuditAction::Transfer,
            Self::SessionResize => AuditAction::Resize,
            Self::SessionAbort => AuditAction::Abort,
            Self::SessionKill => AuditAction::Kill,
            Self::LeaseHeartbeat => AuditAction::Heartbeat,
        }
    }

    /// Whether the method is a state-changing method that requires an audit
    /// admission and, for the privileged subset, a peer re-check.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        !matches!(
            self,
            Self::DaemonNegotiate
                | Self::DaemonStatus
                | Self::SessionStatus
                | Self::SessionList
                | Self::LeaseHeartbeat
        )
    }
}

impl fmt::Display for ServiceMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The result of resolving a caller-supplied method name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodResolution {
    /// A catalog method.
    Known(ServiceMethod),
    /// A well-formed name outside the catalog (`NotFound`, deny-by-default).
    Unknown,
    /// A name that violates the `bitty-ipc` method grammar (`InvalidMethod`).
    Malformed,
}

/// Resolve a method name against the frozen catalog.
///
/// The grammar check reuses `bitty-ipc-api`, so a malformed name is
/// `InvalidMethod` and a well-formed unregistered name is `Unknown` —
/// `NotFound` at the service boundary. Neither is a silent no-op and neither
/// falls through to a default grant.
#[must_use]
pub fn resolve(method: &str) -> MethodResolution {
    if validate_method_name(method).is_err() {
        return MethodResolution::Malformed;
    }
    match ServiceMethod::from_name(method) {
        Some(method) => MethodResolution::Known(method),
        None => MethodResolution::Unknown,
    }
}

/// Whether a resolved service method is one of the v1 families.
///
/// Always `false`: this is the regression guard for "`terminal.*` is never
/// stretched to cover session or daemon administration".
#[must_use]
pub fn is_transport_family(method: &str) -> bool {
    matches!(
        method.split('.').next(),
        Some("terminal" | "view" | "config" | "plugin" | "process" | "debug" | "ai")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_ipc_api::scope::{all_known_methods, required_scope_for_method};

    #[test]
    fn every_scope_spelling_is_frozen_and_unique() {
        let mut names = Vec::new();
        for scope in ServiceScope::ALL {
            assert!(validate_method_name(scope.as_str()).is_ok());
            assert_eq!(
                scope.as_str().split('.').next(),
                Some(scope.family()),
                "{scope} is not in its own family"
            );
            assert!(!names.contains(&scope.as_str()));
            names.push(scope.as_str());
        }
        assert_eq!(
            names,
            [
                "session.inspect",
                "session.host",
                "session.attach",
                "session.control",
                "daemon.inspect",
                "daemon.control",
            ]
        );
    }

    #[test]
    fn baseline_never_contains_session_host_or_daemon_scopes() {
        let baseline = ServiceScopeSet::baseline();
        assert!(baseline.contains(ServiceScope::SessionInspect));
        assert!(baseline.contains(ServiceScope::SessionAttach));
        assert!(baseline.contains(ServiceScope::SessionControl));
        assert!(!baseline.contains(ServiceScope::SessionHost));
        assert!(baseline.iter().all(|scope| !scope.is_operator()));
        assert_eq!(baseline.len(), 3);
        assert_eq!(
            baseline.to_string(),
            "session.inspect session.attach session.control"
        );
    }

    #[test]
    fn operator_scopes_are_exactly_the_daemon_family() {
        let operator = ServiceScopeSet::operator();
        assert_eq!(
            operator.names().collect::<Vec<_>>(),
            ["daemon.inspect", "daemon.control"]
        );
        assert!(operator.iter().all(ServiceScope::is_operator));
        assert!(!operator.contains(ServiceScope::SessionHost));
    }

    #[test]
    fn scope_set_operations_are_bounded_to_the_closed_family() {
        let mut set = ServiceScopeSet::EMPTY;
        assert!(set.is_empty());
        set.insert(ServiceScope::SessionHost);
        assert!(set.contains(ServiceScope::SessionHost));
        assert_eq!(set.len(), 1);
        set.remove(ServiceScope::SessionHost);
        assert!(set.is_empty());
        assert_eq!(ServiceScopeSet::all().len(), ServiceScope::ALL.len());
        assert!(ServiceScopeSet::all().contains_all(ServiceScopeSet::baseline()));
        assert!(!ServiceScopeSet::baseline().contains_all(ServiceScopeSet::operator()));
    }

    #[test]
    fn every_catalog_method_name_is_grammar_valid_and_unique() {
        let mut names = Vec::new();
        for method in ServiceMethod::ALL {
            assert!(
                validate_method_name(method.name()).is_ok(),
                "{} is not a valid method name",
                method.name()
            );
            assert!(!names.contains(&method.name()));
            names.push(method.name());
        }
        assert_eq!(names.len(), ServiceMethod::ALL.len());
        assert_eq!(
            ServiceMethod::from_name("session.grant.revoke"),
            Some(ServiceMethod::SessionGrantRevoke)
        );
    }

    #[test]
    fn catalog_matches_the_frozen_spelling_list() {
        assert_eq!(
            ServiceMethod::ALL.map(ServiceMethod::name),
            [
                "daemon.negotiate",
                "daemon.status",
                "daemon.stop",
                "daemon.host_grant",
                "daemon.orphans.verify",
                "session.create",
                "session.status",
                "session.list",
                "session.attach",
                "session.grant",
                "session.grant.revoke",
                "session.detach",
                "session.control.transfer",
                "session.resize",
                "session.abort",
                "session.kill",
                "lease.heartbeat",
            ]
        );
    }

    #[test]
    fn unknown_methods_are_not_found_and_malformed_methods_are_invalid_method() {
        assert_eq!(
            resolve("session.unknown"),
            MethodResolution::Unknown,
            "a well-formed unknown service method is NotFound"
        );
        assert_eq!(
            resolve("terminal.text"),
            MethodResolution::Unknown,
            "the daemon does not serve v1 methods; they are NotFound"
        );
        assert_eq!(resolve("Session.Create"), MethodResolution::Malformed);
        assert_eq!(resolve("session..create"), MethodResolution::Malformed);
        assert_eq!(resolve(""), MethodResolution::Malformed);
        assert_eq!(
            resolve("session.create"),
            MethodResolution::Known(ServiceMethod::SessionCreate)
        );
    }

    #[test]
    fn the_service_catalog_never_redefines_a_v1_method() {
        for method in all_known_methods() {
            assert!(
                ServiceMethod::from_name(method).is_none(),
                "{method} is a v1 method and must stay owned by bitty-ipc"
            );
            assert!(is_transport_family(method), "{method} is not a v1 family");
        }
        for method in ServiceMethod::ALL {
            assert!(
                required_scope_for_method(method.name()).is_none(),
                "{} must not appear in the v1 method registry",
                method.name()
            );
            assert!(!is_transport_family(method.name()));
        }
    }

    #[test]
    fn scope_requirements_follow_the_frozen_table() {
        assert_eq!(
            ServiceMethod::DaemonNegotiate.scope(),
            ScopeRequirement::None
        );
        assert_eq!(
            ServiceMethod::SessionCreate.scope(),
            ScopeRequirement::Service(&SESSION_HOST)
        );
        assert_eq!(
            ServiceMethod::SessionAttach.scope(),
            ScopeRequirement::Service(&SESSION_ATTACH)
        );
        assert_eq!(
            ServiceMethod::SessionAttach.controller_scope(),
            Some(ServiceScope::SessionControl)
        );
        assert_eq!(ServiceMethod::SessionCreate.controller_scope(), None);
        assert_eq!(
            ServiceMethod::SessionGrant.scope(),
            ScopeRequirement::Service(&SESSION_CONTROL_OR_OPERATOR)
        );
        assert_eq!(
            ServiceMethod::SessionKill.scope(),
            ScopeRequirement::Service(&DAEMON_CONTROL)
        );
        assert_eq!(
            ServiceMethod::SessionResize.scope(),
            ScopeRequirement::Transport(&TERMINAL_MANAGE)
        );
        assert_eq!(
            ServiceMethod::LeaseHeartbeat.scope(),
            ScopeRequirement::None
        );
    }

    #[test]
    fn terminal_manage_is_the_only_transport_requirement() {
        let mut transport = 0;
        for method in ServiceMethod::ALL {
            match method.scope() {
                ScopeRequirement::Transport(scopes) => {
                    transport += 1;
                    assert_eq!(method, ServiceMethod::SessionResize);
                    assert_eq!(scopes, &[TransportScope::TerminalManage]);
                }
                ScopeRequirement::None | ScopeRequirement::Service(_) => {}
            }
        }
        assert_eq!(transport, 1);
    }

    #[test]
    fn every_boundary_is_reachable_from_the_catalog() {
        let mut mapped = Vec::new();
        for method in ServiceMethod::ALL {
            for boundary in method.boundaries() {
                assert!(
                    !mapped.contains(boundary),
                    "{boundary} is mapped from two methods"
                );
                mapped.push(*boundary);
            }
        }
        for boundary in DispatchBoundary::ALL {
            assert!(
                mapped.contains(&boundary),
                "{boundary} has no catalog method"
            );
        }
        assert_eq!(mapped.len(), DispatchBoundary::ALL.len());
        assert_eq!(
            ServiceMethod::DaemonHostGrant.boundary(),
            Some(DispatchBoundary::HostGrantMint)
        );
        assert_eq!(
            ServiceMethod::SessionGrantRevoke.boundary(),
            Some(DispatchBoundary::ObserverGrantRevoke)
        );
    }

    #[test]
    fn idempotency_classes_match_the_frozen_table() {
        assert_eq!(
            ServiceMethod::SessionCreate.idempotency(),
            IdempotencyClass::Keyed
        );
        assert_eq!(
            ServiceMethod::SessionAttach.idempotency(),
            IdempotencyClass::Keyed
        );
        assert_eq!(
            ServiceMethod::SessionGrant.idempotency(),
            IdempotencyClass::Keyed
        );
        assert_eq!(
            ServiceMethod::SessionControlTransfer.idempotency(),
            IdempotencyClass::Keyed
        );
        assert_eq!(
            ServiceMethod::SessionAbort.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::SessionKill.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::DaemonStop.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::SessionGrantRevoke.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::SessionDetach.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::LeaseHeartbeat.idempotency(),
            IdempotencyClass::Keyless
        );
        assert_eq!(
            ServiceMethod::SessionResize.idempotency(),
            IdempotencyClass::UnkeyedOpen
        );
        assert_eq!(
            ServiceMethod::DaemonHostGrant.idempotency(),
            IdempotencyClass::UnkeyedOpen
        );
    }

    #[test]
    fn mutating_methods_are_the_state_changing_subset() {
        for method in ServiceMethod::ALL {
            let expected = !matches!(
                method,
                ServiceMethod::DaemonNegotiate
                    | ServiceMethod::DaemonStatus
                    | ServiceMethod::SessionStatus
                    | ServiceMethod::SessionList
                    | ServiceMethod::LeaseHeartbeat
            );
            assert_eq!(method.is_mutating(), expected, "{method}");
        }
    }
}
