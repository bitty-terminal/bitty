//! Compiled resource limits and the advertised `{effective, default, system_max}`
//! triples (contract `D-14`, bitty#2603).
//!
//! The table below is the single compiled source of truth for every numeric
//! bound in the accepted [`bittyd` Session-Host Contract]. The 46 rows of the
//! `D-14` table keep their contract order; the eight transport bounds inherited
//! from the accepted `bitty-ipc` v1 `RC-9`/`RC-10` constraints follow in the
//! prose order of that paragraph (the contract defines no normative order for
//! them), because the handshake advertises them through the same schema with
//! `effective == default == system_max`.
//!
//! # Configuration rule
//!
//! [`Limits::new`] starts from the compiled defaults and applies caller
//! overrides. An override may lower a mutable bound or raise it back up to the
//! compiled [`LimitId::system_max_value`], and never above it. Every refusal is
//! a typed [`LimitsError`]; a rejected configuration never produces a partially
//! applied [`Limits`]. Inherited transport bounds are fixed: an override that
//! changes one is [`LimitsError::FixedLimit`].
//!
//! # Enforcement order (normative for bitty#2607)
//!
//! A limit is evaluated at the stage reported by [`LimitId::stage`]. Stages
//! that can refuse a request are ordered before the structure they protect is
//! mutated:
//!
//! 1. [`EnforcementStage::ConnectionAdmission`] — inherited frame, chunk,
//!    payload, pending-request, channel, rate, connection, and per-connection
//!    pending-byte bounds; a connection over a bound is refused or shed before
//!    a request is decoded.
//! 2. [`EnforcementStage::Handshake`] — `pre_auth_max_bytes`; exceeding it
//!    closes the connection before any session operation.
//! 3. [`EnforcementStage::RequestAdmission`] — the `terminal_id_bytes`,
//!    `attach_id_bytes`, and `grant_id_bytes` shape bounds; a malformed
//!    identifier is refused before any lookup.
//! 4. [`EnforcementStage::AuditSink`] — `audit_file_bytes`, `audit_segments`,
//!    and `refusal_audit_budget_per_sec`. The sink availability/rotation check
//!    runs here, **before** a privileged mutation (`D-16`: an unavailable sink
//!    refuses the action with `Internal` and changes no state). The refusal
//!    record for a refused action is appended after the refusal decision; that
//!    append is the only write a refusal performs.
//! 5. [`EnforcementStage::Idempotency`] — the per-session and create-key store
//!    caps and the create-key retention floor; a full store refuses new keyed
//!    work before the executor runs.
//! 6. [`EnforcementStage::RegistryAdmission`] — `max_sessions` and
//!    `session_record_bytes`; a breach refuses before a record is inserted or
//!    transitioned.
//! 7. [`EnforcementStage::SessionMutation`] — attach caps, grant caps, and
//!    spawn-spec bounds; a breach refuses before a lease, grant, spawn, or
//!    transition.
//! 8. [`EnforcementStage::ContentPlane`] — `max_content_bytes_per_session`.
//!    This is not a whole-request refusal: the content owner prunes grid,
//!    scrollback, and images to fit the per-session cap.
//! 9. [`EnforcementStage::Retention`] — `max_retained_terminal_records` and
//!    `terminal_retention_ms` (and orphan quarantine retention). Bounded
//!    records are collected/evicted oldest-first; a cap breach is not a
//!    request refusal.
//! 10. [`EnforcementStage::EventDelivery`] — event backlog, lease queue, and
//!     connection outbound bounds; a breach sheds or closes before an event is
//!     appended or a response byte is queued.
//! 11. [`EnforcementStage::Containment`] — orphan-journal bounds; a breach
//!     refuses a spawn before the journal record is written.
//! 12. [`EnforcementStage::Deadline`] — heartbeat, lease, grant, request,
//!     handshake, idle, spawn-bind, abort, drain, and reap timers. These are
//!     supervisor/timer bounds, not pre-mutation refusal caps; they decide when
//!     a lease expires, a request times out, or a grace period escalates. They
//!     produce timeouts and expiry transitions, never a `LimitExceeded`
//!     refusal by themselves.
//!
//! A refusal carries the limit and the observed count (`D-14`), applies nothing
//! to the registry, event ring, lease store, grant store, or idempotency store,
//! and is the only write allowed to the audit ring (its own refusal record).
//! Termination and revocation are keyless and never consult the idempotency
//! store, so no keyed capacity refusal can block them (`D-18`).
//!
//! [`bittyd` Session-Host Contract]: https://github.com/bitty-terminal/bitty-terminal-docs/blob/main/specifications/bittyd-session-host-contract.md

use std::fmt;

/// Accounted bytes for one in-memory audit record.
///
/// `D-16` bounds every audit string to 128 bytes and the ring is a fixed-size
/// record set; this compiled value is the per-record budget the ring capacity
/// divides `audit_file_bytes × audit_segments` by. It also sets the compiled
/// floor for `audit_file_bytes`, so even the smallest accepted configuration
/// holds one record.
pub const AUDIT_RECORD_BYTES: usize = 320;

/// Accounted bytes for one per-session idempotency descriptor (`D-18`:
/// 128 bytes per entry, 512 entries, 65,536 bytes).
pub const SESSION_IDEMPOTENCY_ENTRY_BYTES: usize = 128;

/// Accounted bytes for one pre-session create-key descriptor (`D-18`:
/// 128 records at 256 bytes each, 32,768 bytes).
pub const CREATE_IDEMPOTENCY_ENTRY_BYTES: usize = 256;

/// Where a limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// A row of the normative `D-14` table.
    ContractD14,
    /// An inherited `bitty-ipc` v1 `RC-9`/`RC-10` transport bound, published
    /// with `effective == default == system_max`.
    InheritedTransport,
}

impl LimitSource {
    /// Static spelling for diagnostics and the conformance corpus.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContractD14 => "d14",
            Self::InheritedTransport => "inherited",
        }
    }
}

/// The point in request handling where a limit is enforced.
///
/// The variants are declared in enforcement order; a service layer evaluates
/// them in ascending order and must not mutate the protected structure before
/// the stage that owns the limit has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EnforcementStage {
    /// Transport admission, before a request is decoded.
    ConnectionAdmission,
    /// Pre-auth handshake bounds.
    Handshake,
    /// Request shape bounds, before any lookup.
    RequestAdmission,
    /// Audit sink availability and rotation, before a privileged mutation.
    AuditSink,
    /// Idempotency-store admission, before the executor runs.
    Idempotency,
    /// Session-registry admission, before a record changes.
    RegistryAdmission,
    /// Lease, grant, and spawn mutation guards.
    SessionMutation,
    /// Content-plane pruning; never a whole-request refusal.
    ContentPlane,
    /// Bounded record collection/eviction; never a request refusal.
    Retention,
    /// Event backlog, lease queue, and outbound buffering.
    EventDelivery,
    /// Orphan-journal containment bounds.
    Containment,
    /// Timer and deadline bounds; not pre-mutation refusal caps.
    Deadline,
}

impl EnforcementStage {
    /// Static spelling for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectionAdmission => "connection_admission",
            Self::Handshake => "handshake",
            Self::RequestAdmission => "request_admission",
            Self::AuditSink => "audit_sink",
            Self::Idempotency => "idempotency",
            Self::RegistryAdmission => "registry_admission",
            Self::SessionMutation => "session_mutation",
            Self::ContentPlane => "content_plane",
            Self::Retention => "retention",
            Self::EventDelivery => "event_delivery",
            Self::Containment => "containment",
            Self::Deadline => "deadline",
        }
    }
}

impl fmt::Display for EnforcementStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One compiled limit row.
#[derive(Debug, Clone, Copy)]
struct LimitSpec {
    name: &'static str,
    default: u64,
    system_max: u64,
    source: LimitSource,
    stage: EnforcementStage,
}

macro_rules! define_limits {
    ($( $variant:ident => ($name:literal, $default:literal, $max:literal, $source:ident, $stage:ident), )*) => {
        /// Identifier of one advertised limit, in contract order.
        ///
        /// The discriminants are the row indices in the compiled table; the
        /// declaration order is the `D-14` table order followed by the
        /// inherited transport bounds. The inherited rows have no normative
        /// order in the contract; they follow the prose order of the inherited
        /// transport paragraph here for deterministic advertisement.
        #[repr(usize)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum LimitId {
            $(
                #[doc = $name]
                $variant,
            )*
        }

        /// Number of compiled limits.
        pub const LIMIT_COUNT: usize = [$(stringify!($variant)),*].len();

        impl LimitId {
            /// Every limit in contract order.
            pub const ALL: [Self; LIMIT_COUNT] = [$(Self::$variant),*];
        }

        const LIMIT_TABLE: [LimitSpec; LIMIT_COUNT] = [
            $(
                LimitSpec {
                    name: $name,
                    default: $default,
                    system_max: $max,
                    source: LimitSource::$source,
                    stage: EnforcementStage::$stage,
                },
            )*
        ];
    };
}

define_limits! {
    MaxSessions => ("max_sessions", 16, 64, ContractD14, RegistryAdmission),
    MaxAttachesPerSession => ("max_attaches_per_session", 8, 16, ContractD14, SessionMutation),
    MaxAttachesPerConnection => ("max_attaches_per_connection", 8, 16, ContractD14, SessionMutation),
    MaxRetainedTerminalRecords => ("max_retained_terminal_records", 64, 64, ContractD14, Retention),
    SessionRecordBytes => ("session_record_bytes", 4096, 4096, ContractD14, RegistryAdmission),
    TerminalRetentionMs => ("terminal_retention_ms", 600_000, 3_600_000, ContractD14, Retention),
    MaxEventBacklogEvents => ("max_event_backlog_events", 256, 256, ContractD14, EventDelivery),
    MaxEventBacklogBytes => ("max_event_backlog_bytes", 262_144, 262_144, ContractD14, EventDelivery),
    MaxLeaseQueueEvents => ("max_lease_queue_events", 256, 256, ContractD14, EventDelivery),
    MaxLeaseQueueBytes => ("max_lease_queue_bytes", 65_536, 65_536, ContractD14, EventDelivery),
    MaxConnectionPendingBytes => ("max_connection_pending_bytes", 1_048_576, 1_048_576, ContractD14, ConnectionAdmission),
    MaxConnectionOutboundBytes => ("max_connection_outbound_bytes", 1_048_576, 1_048_576, ContractD14, EventDelivery),
    AttachHeartbeatIntervalMs => ("attach_heartbeat_interval_ms", 5_000, 5_000, ContractD14, Deadline),
    AttachLeaseTtlMs => ("attach_lease_ttl_ms", 15_000, 15_000, ContractD14, Deadline),
    ObserverGrantTtlMs => ("observer_grant_ttl_ms", 60_000, 300_000, ContractD14, Deadline),
    MaxPendingObserverGrantsPerSession => ("max_pending_observer_grants_per_session", 8, 8, ContractD14, SessionMutation),
    HostGrantTtlMs => ("host_grant_ttl_ms", 300_000, 900_000, ContractD14, Deadline),
    MaxPendingHostGrants => ("max_pending_host_grants", 8, 8, ContractD14, SessionMutation),
    HostGrantRecordBytes => ("host_grant_record_bytes", 128, 128, ContractD14, SessionMutation),
    GrantIdBytes => ("grant_id_bytes", 32, 32, ContractD14, RequestAdmission),
    TransportIdleMs => ("transport_idle_ms", 30_000, 30_000, ContractD14, Deadline),
    RequestTimeoutMs => ("request_timeout_ms", 10_000, 10_000, ContractD14, Deadline),
    NegotiateTimeoutMs => ("negotiate_timeout_ms", 5_000, 5_000, ContractD14, Deadline),
    PreAuthMaxBytes => ("pre_auth_max_bytes", 8_192, 8_192, ContractD14, Handshake),
    SpawnBindTimeoutMs => ("spawn_bind_timeout_ms", 30_000, 30_000, ContractD14, Deadline),
    AbortGraceMs => ("abort_grace_ms", 5_000, 5_000, ContractD14, Deadline),
    DrainGraceMs => ("drain_grace_ms", 10_000, 10_000, ContractD14, Deadline),
    OrphanReapGraceMs => ("orphan_reap_grace_ms", 5_000, 5_000, ContractD14, Deadline),
    OrphanRecordRetentionMs => ("orphan_record_retention_ms", 3_600_000, 86_400_000, ContractD14, Retention),
    MaxIdempotencyKeysPerSession => ("max_idempotency_keys_per_session", 512, 512, ContractD14, Idempotency),
    MaxIdempotencyBytesPerSession => ("max_idempotency_bytes_per_session", 65_536, 65_536, ContractD14, Idempotency),
    MaxCreateIdempotencyKeys => ("max_create_idempotency_keys", 128, 128, ContractD14, Idempotency),
    MaxCreateIdempotencyBytes => ("max_create_idempotency_bytes", 32_768, 32_768, ContractD14, Idempotency),
    CreateIdempotencyTtlMs => ("create_idempotency_ttl_ms", 600_000, 3_600_000, ContractD14, Idempotency),
    MaxOrphanRecords => ("max_orphan_records", 64, 64, ContractD14, Containment),
    OrphanRecordBytes => ("orphan_record_bytes", 4_096, 4_096, ContractD14, Containment),
    TerminalIdBytes => ("terminal_id_bytes", 128, 128, ContractD14, RequestAdmission),
    AttachIdBytes => ("attach_id_bytes", 32, 32, ContractD14, RequestAdmission),
    RefusalAuditBudgetPerSec => ("refusal_audit_budget_per_sec", 20, 20, ContractD14, AuditSink),
    AuditFileBytes => ("audit_file_bytes", 1_048_576, 2_097_152, ContractD14, AuditSink),
    AuditSegments => ("audit_segments", 2, 2, ContractD14, AuditSink),
    MaxSpawnArgvEntries => ("max_spawn_argv_entries", 64, 64, ContractD14, SessionMutation),
    MaxSpawnEnvEntries => ("max_spawn_env_entries", 128, 128, ContractD14, SessionMutation),
    MaxSpawnFieldBytes => ("max_spawn_field_bytes", 4_096, 4_096, ContractD14, SessionMutation),
    MaxSpawnSpecBytes => ("max_spawn_spec_bytes", 65_536, 65_536, ContractD14, SessionMutation),
    MaxContentBytesPerSession => ("max_content_bytes_per_session", 268_435_456, 268_435_456, ContractD14, ContentPlane),
    MaxFrameBytes => ("max_frame_bytes", 262_144, 262_144, InheritedTransport, ConnectionAdmission),
    MaxChunkBytes => ("max_chunk_bytes", 262_144, 262_144, InheritedTransport, ConnectionAdmission),
    MaxPendingRequests => ("max_pending_requests", 64, 64, InheritedTransport, ConnectionAdmission),
    MaxChannelCapacity => ("max_channel_capacity", 256, 256, InheritedTransport, ConnectionAdmission),
    MaxRequestRatePerSec => ("max_request_rate_per_sec", 100, 100, InheritedTransport, ConnectionAdmission),
    MaxRequestBurst => ("max_request_burst", 200, 200, InheritedTransport, ConnectionAdmission),
    MaxConnectionsPerEndpoint => ("max_connections_per_endpoint", 16, 16, InheritedTransport, ConnectionAdmission),
    MaxRequestPayloadBytes => ("max_request_payload_bytes", 1_048_576, 1_048_576, InheritedTransport, ConnectionAdmission),
}

impl LimitId {
    /// Resolve a contract name to its compiled limit.
    ///
    /// The lookup is exact: an unknown or differently spelled name is `None`,
    /// never a closest match, so a configuration typo cannot silently change a
    /// different bound.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|limit| limit.name() == name)
    }

    /// Contract name used in the advertised `limits` object.
    #[must_use]
    pub const fn name(self) -> &'static str {
        LIMIT_TABLE[self as usize].name
    }

    /// Compiled default.
    #[must_use]
    pub const fn default_value(self) -> u64 {
        LIMIT_TABLE[self as usize].default
    }

    /// Compiled ceiling; raising it requires a contract revision plus security
    /// review (`D-14`).
    #[must_use]
    pub const fn system_max_value(self) -> u64 {
        LIMIT_TABLE[self as usize].system_max
    }

    /// Where the row came from.
    #[must_use]
    pub const fn source(self) -> LimitSource {
        LIMIT_TABLE[self as usize].source
    }

    /// Enforcement stage; see the module documentation for the order.
    #[must_use]
    pub const fn stage(self) -> EnforcementStage {
        LIMIT_TABLE[self as usize].stage
    }

    /// Whether this is an inherited transport bound that cannot be configured.
    #[must_use]
    pub const fn is_fixed(self) -> bool {
        matches!(self.source(), LimitSource::InheritedTransport)
    }

    /// Smallest value a configuration may set (`D-14`: "never below a compiled
    /// floor where one is defined").
    ///
    /// Most mutable limits have a floor of `1`, so a configuration cannot
    /// silently disable the mechanism. The two idempotency byte bounds have
    /// the compiled descriptor budget as their floor, and `audit_file_bytes`
    /// has the compiled audit-record budget, so the smallest accepted
    /// configuration still holds one entry. The fixed-format identifier bounds
    /// (`terminal_id_bytes`, `attach_id_bytes`, `grant_id_bytes`) have their
    /// system maximum as the floor: the registry and wire formats enforce the
    /// fixed widths, so a lowered advertisement could not be honored. Fixed
    /// transport bounds have their own value as the floor.
    #[must_use]
    pub const fn floor(self) -> u64 {
        if self.is_fixed() {
            return self.default_value();
        }
        match self {
            Self::MaxIdempotencyBytesPerSession => SESSION_IDEMPOTENCY_ENTRY_BYTES as u64,
            Self::MaxCreateIdempotencyBytes => CREATE_IDEMPOTENCY_ENTRY_BYTES as u64,
            Self::AuditFileBytes => AUDIT_RECORD_BYTES as u64,
            Self::TerminalIdBytes | Self::AttachIdBytes | Self::GrantIdBytes => {
                self.system_max_value()
            }
            _ => 1,
        }
    }
}

impl fmt::Display for LimitId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One advertised `{effective, default, system_max}` triple.
///
/// Construction is private: only [`Limits`] builds a value, so the invariant
/// `floor <= effective <= system_max` cannot be bypassed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LimitValue {
    effective: u64,
    default: u64,
    system_max: u64,
}

impl LimitValue {
    /// The value the daemon enforces for this run.
    #[must_use]
    pub const fn effective(self) -> u64 {
        self.effective
    }

    /// The compiled default.
    #[must_use]
    pub const fn default_value(self) -> u64 {
        self.default
    }

    /// The compiled ceiling.
    #[must_use]
    pub const fn system_max(self) -> u64 {
        self.system_max
    }

    /// The triple in `{effective, default, system_max}` order.
    #[must_use]
    pub const fn triple(self) -> (u64, u64, u64) {
        (self.effective, self.default, self.system_max)
    }
}

/// One advertised limit: the contract name and its triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisedLimit {
    /// Contract name (`D-14` spelling, or the local spelling for an inherited
    /// `RC-9`/`RC-10` bound).
    pub name: &'static str,
    /// The advertised triple.
    pub value: LimitValue,
}

/// One configuration override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitOverride {
    limit: LimitId,
    effective: u64,
}

impl LimitOverride {
    /// Request `effective` for `limit`.
    #[must_use]
    pub const fn new(limit: LimitId, effective: u64) -> Self {
        Self { limit, effective }
    }

    /// The limit being configured.
    #[must_use]
    pub const fn limit(self) -> LimitId {
        self.limit
    }

    /// The requested value.
    #[must_use]
    pub const fn effective(self) -> u64 {
        self.effective
    }
}

/// A refused limit configuration. No variant carries a caller-supplied string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {
    /// The requested value is above the compiled system maximum.
    AboveSystemMax {
        /// The configured limit.
        limit: LimitId,
        /// The requested value.
        requested: u64,
        /// The compiled ceiling.
        system_max: u64,
    },
    /// The requested value is below the compiled floor.
    BelowFloor {
        /// The configured limit.
        limit: LimitId,
        /// The requested value.
        requested: u64,
        /// The compiled floor.
        floor: u64,
    },
    /// An inherited transport bound cannot be configured away from its value.
    FixedLimit {
        /// The configured limit.
        limit: LimitId,
        /// The requested value.
        requested: u64,
        /// The only accepted value.
        fixed: u64,
    },
    /// The same limit appeared more than once in one configuration.
    DuplicateOverride {
        /// The repeated limit.
        limit: LimitId,
    },
}

impl fmt::Display for LimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AboveSystemMax {
                limit,
                requested,
                system_max,
            } => write!(
                formatter,
                "limit {limit} requested {requested}, above compiled maximum {system_max}"
            ),
            Self::BelowFloor {
                limit,
                requested,
                floor,
            } => write!(
                formatter,
                "limit {limit} requested {requested}, below compiled floor {floor}"
            ),
            Self::FixedLimit {
                limit,
                requested,
                fixed,
            } => write!(
                formatter,
                "limit {limit} is fixed at {fixed}, not {requested}"
            ),
            Self::DuplicateOverride { limit } => {
                write!(formatter, "limit {limit} is configured more than once")
            }
        }
    }
}

impl std::error::Error for LimitsError {}

/// A whole-request limit refusal: which limit, its enforced value, and the
/// observed count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitRefusal {
    limit: LimitId,
    effective: u64,
    actual: u64,
}

impl LimitRefusal {
    /// Build a refusal from an enforced value and the observed count.
    #[must_use]
    pub const fn new(limit: LimitId, effective: u64, actual: u64) -> Self {
        Self {
            limit,
            effective,
            actual,
        }
    }

    /// The limit that was hit.
    #[must_use]
    pub const fn limit(self) -> LimitId {
        self.limit
    }

    /// The enforced value for this run.
    #[must_use]
    pub const fn effective(self) -> u64 {
        self.effective
    }

    /// The observed count or size that exceeded the limit.
    #[must_use]
    pub const fn actual(self) -> u64 {
        self.actual
    }
}

impl fmt::Display for LimitRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "limit {} is {}, observed {}",
            self.limit, self.effective, self.actual
        )
    }
}

/// The effective limits for one daemon run.
///
/// The default value is the compiled configuration: every `effective` equals
/// its compiled default. [`Self::new`] applies overrides within the compiled
/// range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    values: [LimitValue; LIMIT_COUNT],
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            values: std::array::from_fn(|index| {
                let spec = &LIMIT_TABLE[index];
                LimitValue {
                    effective: spec.default,
                    default: spec.default,
                    system_max: spec.system_max,
                }
            }),
        }
    }
}

impl Limits {
    /// Build effective limits from the compiled defaults plus `overrides`.
    ///
    /// # Errors
    ///
    /// Refuses the whole configuration, without returning a partially applied
    /// value, when an override repeats a limit
    /// ([`LimitsError::DuplicateOverride`]), exceeds the compiled maximum
    /// ([`LimitsError::AboveSystemMax`]), falls below the compiled floor
    /// ([`LimitsError::BelowFloor`]), or attempts to change a fixed inherited
    /// transport bound ([`LimitsError::FixedLimit`]).
    pub fn new(overrides: &[LimitOverride]) -> Result<Self, LimitsError> {
        let mut seen = [false; LIMIT_COUNT];
        for request in overrides {
            let index = request.limit as usize;
            if seen[index] {
                return Err(LimitsError::DuplicateOverride {
                    limit: request.limit,
                });
            }
            seen[index] = true;

            if request.limit.is_fixed() {
                let fixed = request.limit.default_value();
                if request.effective != fixed {
                    return Err(LimitsError::FixedLimit {
                        limit: request.limit,
                        requested: request.effective,
                        fixed,
                    });
                }
                continue;
            }
            if request.effective > request.limit.system_max_value() {
                return Err(LimitsError::AboveSystemMax {
                    limit: request.limit,
                    requested: request.effective,
                    system_max: request.limit.system_max_value(),
                });
            }
            if request.effective < request.limit.floor() {
                return Err(LimitsError::BelowFloor {
                    limit: request.limit,
                    requested: request.effective,
                    floor: request.limit.floor(),
                });
            }
        }

        let mut limits = Self::default();
        for request in overrides {
            let value = &mut limits.values[request.limit as usize];
            value.effective = request.effective;
        }
        Ok(limits)
    }

    /// The advertised triple for `limit`.
    #[must_use]
    pub fn get(&self, limit: LimitId) -> LimitValue {
        self.values[limit as usize]
    }

    /// The effective value for `limit`.
    #[must_use]
    pub fn effective(&self, limit: LimitId) -> u64 {
        self.get(limit).effective()
    }

    /// The effective value as a `usize`.
    ///
    /// Every compiled limit fits a 32-bit `usize` at the system maximum; the
    /// saturating conversion is defensive only.
    #[must_use]
    pub fn effective_usize(&self, limit: LimitId) -> usize {
        usize::try_from(self.effective(limit)).unwrap_or(usize::MAX)
    }

    /// The compiled default for `limit`.
    #[must_use]
    pub fn default_value(&self, limit: LimitId) -> u64 {
        self.get(limit).default_value()
    }

    /// The compiled system maximum for `limit`.
    #[must_use]
    pub fn system_max(&self, limit: LimitId) -> u64 {
        self.get(limit).system_max()
    }

    /// Every advertised limit in contract order.
    pub fn advertised(&self) -> impl Iterator<Item = AdvertisedLimit> + '_ {
        LimitId::ALL.into_iter().map(|limit| AdvertisedLimit {
            name: limit.name(),
            value: self.get(limit),
        })
    }

    /// Whether every effective value equals its compiled default.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.values
            .iter()
            .all(|value| value.effective == value.default)
    }

    /// Build a whole-request refusal carrying the enforced value of `limit`.
    #[must_use]
    pub fn refusal(&self, limit: LimitId, actual: u64) -> LimitRefusal {
        LimitRefusal::new(limit, self.effective(limit), actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_order_matches_the_enum_discriminants() {
        assert_eq!(LIMIT_COUNT, 54);
        assert_eq!(LimitId::ALL.len(), LIMIT_COUNT);
        for (index, limit) in LimitId::ALL.into_iter().enumerate() {
            assert_eq!(limit as usize, index, "{limit}");
            assert_eq!(limit.name(), LIMIT_TABLE[index].name);
        }
        let names: std::collections::BTreeSet<_> =
            LimitId::ALL.into_iter().map(LimitId::name).collect();
        assert_eq!(names.len(), LIMIT_COUNT);
    }

    #[test]
    fn d14_rows_precede_the_inherited_transport_bounds() {
        let d14 = LimitId::ALL
            .into_iter()
            .filter(|limit| !limit.is_fixed())
            .count();
        assert_eq!(d14, 46);
        assert_eq!(LimitId::ALL.len() - d14, 8);
        assert_eq!(LimitId::MaxSessions.source(), LimitSource::ContractD14);
        assert_eq!(
            LimitId::MaxConnectionsPerEndpoint.source(),
            LimitSource::InheritedTransport
        );
        for limit in LimitId::ALL.into_iter().filter(|limit| limit.is_fixed()) {
            assert_eq!(limit.default_value(), limit.system_max_value());
            assert_eq!(limit.floor(), limit.default_value());
        }
    }

    #[test]
    fn names_resolve_exactly() {
        assert_eq!(
            LimitId::from_name("max_sessions"),
            Some(LimitId::MaxSessions)
        );
        assert_eq!(
            LimitId::from_name("max_connections_per_endpoint"),
            Some(LimitId::MaxConnectionsPerEndpoint)
        );
        assert_eq!(LimitId::from_name("max_sessions "), None);
        assert_eq!(LimitId::from_name("MAX_SESSIONS"), None);
        assert_eq!(LimitId::from_name("unknown"), None);
    }

    #[test]
    fn from_name_round_trips_every_compiled_name() {
        assert_eq!(LimitId::ALL.len(), 54);
        for limit in LimitId::ALL {
            assert_eq!(LimitId::from_name(limit.name()), Some(limit));
        }
    }

    #[test]
    fn compiled_defaults_and_maxima_are_ordered() {
        for limit in LimitId::ALL {
            assert!(
                limit.floor() <= limit.default_value(),
                "{limit} floor above default"
            );
            assert!(
                limit.default_value() <= limit.system_max_value(),
                "{limit} default above maximum"
            );
        }
        assert_eq!(LimitId::MaxSessions.default_value(), 16);
        assert_eq!(LimitId::MaxSessions.system_max_value(), 64);
        assert_eq!(LimitId::AuditFileBytes.default_value(), 1_048_576);
        assert_eq!(LimitId::AuditFileBytes.system_max_value(), 2_097_152);
        assert_eq!(LimitId::ObserverGrantTtlMs.default_value(), 60_000);
        assert_eq!(LimitId::ObserverGrantTtlMs.system_max_value(), 300_000);
    }

    #[test]
    fn every_limit_maps_to_an_enforcement_stage() {
        for limit in LimitId::ALL {
            let stage = limit.stage();
            assert!(!stage.as_str().is_empty());
        }
        assert_eq!(
            LimitId::PreAuthMaxBytes.stage(),
            EnforcementStage::Handshake
        );
        assert_eq!(
            LimitId::MaxConnectionsPerEndpoint.stage(),
            EnforcementStage::ConnectionAdmission
        );
        assert_eq!(
            LimitId::MaxOrphanRecords.stage(),
            EnforcementStage::Containment
        );
        assert_eq!(LimitId::AuditSegments.stage(), EnforcementStage::AuditSink);
        assert_eq!(
            LimitId::MaxContentBytesPerSession.stage(),
            EnforcementStage::ContentPlane
        );
        assert_eq!(
            LimitId::MaxRetainedTerminalRecords.stage(),
            EnforcementStage::Retention
        );
        assert_eq!(
            LimitId::AttachLeaseTtlMs.stage(),
            EnforcementStage::Deadline
        );
        assert!(EnforcementStage::ConnectionAdmission < EnforcementStage::Handshake);
        assert!(EnforcementStage::Handshake < EnforcementStage::RequestAdmission);
        assert!(EnforcementStage::RequestAdmission < EnforcementStage::AuditSink);
        assert!(EnforcementStage::AuditSink < EnforcementStage::Idempotency);
        assert!(EnforcementStage::Idempotency < EnforcementStage::RegistryAdmission);
        assert!(EnforcementStage::RegistryAdmission < EnforcementStage::SessionMutation);
        assert!(EnforcementStage::SessionMutation < EnforcementStage::ContentPlane);
        assert!(EnforcementStage::ContentPlane < EnforcementStage::Retention);
        assert!(EnforcementStage::Retention < EnforcementStage::EventDelivery);
        assert!(EnforcementStage::EventDelivery < EnforcementStage::Containment);
        assert!(EnforcementStage::Containment < EnforcementStage::Deadline);
    }

    #[test]
    fn default_limits_advertise_the_compiled_table() {
        let limits = Limits::default();
        assert!(limits.is_default());
        let advertised: Vec<_> = limits.advertised().collect();
        assert_eq!(advertised.len(), LIMIT_COUNT);
        for entry in advertised {
            let limit = LimitId::ALL
                .into_iter()
                .find(|limit| limit.name() == entry.name)
                .expect("advertised name is a compiled limit");
            assert_eq!(entry.value.effective(), limit.default_value());
            assert_eq!(entry.value.default_value(), limit.default_value());
            assert_eq!(entry.value.system_max(), limit.system_max_value());
            assert_eq!(
                entry.value.triple(),
                (
                    limit.default_value(),
                    limit.default_value(),
                    limit.system_max_value()
                )
            );
        }
    }

    #[test]
    fn overrides_may_raise_to_the_maximum_but_not_above_it() {
        let at_max = Limits::new(&[LimitOverride::new(
            LimitId::MaxSessions,
            LimitId::MaxSessions.system_max_value(),
        )])
        .expect("system maximum is accepted");
        assert_eq!(
            at_max.effective(LimitId::MaxSessions),
            LimitId::MaxSessions.system_max_value()
        );
        assert!(!at_max.is_default());

        assert_eq!(
            Limits::new(&[LimitOverride::new(
                LimitId::MaxSessions,
                LimitId::MaxSessions.system_max_value() + 1
            )]),
            Err(LimitsError::AboveSystemMax {
                limit: LimitId::MaxSessions,
                requested: 65,
                system_max: 64,
            })
        );
    }

    #[test]
    fn overrides_may_lower_to_the_floor_but_not_below_it() {
        let at_floor =
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 1)]).expect("floor is accepted");
        assert_eq!(at_floor.effective(LimitId::MaxSessions), 1);

        assert_eq!(
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 0)]),
            Err(LimitsError::BelowFloor {
                limit: LimitId::MaxSessions,
                requested: 0,
                floor: 1,
            })
        );
    }

    #[test]
    fn compiled_budget_floors_are_enforced() {
        assert_eq!(
            Limits::new(&[LimitOverride::new(
                LimitId::MaxIdempotencyBytesPerSession,
                (SESSION_IDEMPOTENCY_ENTRY_BYTES - 1) as u64
            )]),
            Err(LimitsError::BelowFloor {
                limit: LimitId::MaxIdempotencyBytesPerSession,
                requested: 127,
                floor: 128,
            })
        );
        assert_eq!(
            Limits::new(&[LimitOverride::new(
                LimitId::MaxCreateIdempotencyBytes,
                (CREATE_IDEMPOTENCY_ENTRY_BYTES - 1) as u64
            )]),
            Err(LimitsError::BelowFloor {
                limit: LimitId::MaxCreateIdempotencyBytes,
                requested: 255,
                floor: 256,
            })
        );
        assert_eq!(
            Limits::new(&[LimitOverride::new(
                LimitId::AuditFileBytes,
                (AUDIT_RECORD_BYTES - 1) as u64
            )]),
            Err(LimitsError::BelowFloor {
                limit: LimitId::AuditFileBytes,
                requested: 319,
                floor: 320,
            })
        );
    }

    #[test]
    fn fixed_format_identifier_bounds_are_not_lowerable() {
        for limit in [
            LimitId::TerminalIdBytes,
            LimitId::AttachIdBytes,
            LimitId::GrantIdBytes,
        ] {
            assert_eq!(limit.floor(), limit.system_max_value());
            assert_eq!(
                Limits::new(&[LimitOverride::new(limit, limit.system_max_value())])
                    .expect("the fixed value is accepted")
                    .effective(limit),
                limit.system_max_value()
            );
            assert_eq!(
                Limits::new(&[LimitOverride::new(limit, limit.system_max_value() - 1)]),
                Err(LimitsError::BelowFloor {
                    limit,
                    requested: limit.system_max_value() - 1,
                    floor: limit.system_max_value(),
                })
            );
        }
    }

    #[test]
    fn inherited_transport_bounds_are_fixed() {
        let fixed = Limits::new(&[LimitOverride::new(
            LimitId::MaxFrameBytes,
            LimitId::MaxFrameBytes.default_value(),
        )])
        .expect("the fixed value is accepted");
        assert_eq!(
            fixed.effective(LimitId::MaxFrameBytes),
            LimitId::MaxFrameBytes.default_value()
        );

        for requested in [
            LimitId::MaxFrameBytes.default_value() - 1,
            LimitId::MaxFrameBytes.default_value() + 1,
        ] {
            assert_eq!(
                Limits::new(&[LimitOverride::new(LimitId::MaxFrameBytes, requested)]),
                Err(LimitsError::FixedLimit {
                    limit: LimitId::MaxFrameBytes,
                    requested,
                    fixed: LimitId::MaxFrameBytes.default_value(),
                })
            );
        }
    }

    #[test]
    fn duplicate_overrides_are_refused() {
        assert_eq!(
            Limits::new(&[
                LimitOverride::new(LimitId::MaxSessions, 8),
                LimitOverride::new(LimitId::MaxSessions, 4),
            ]),
            Err(LimitsError::DuplicateOverride {
                limit: LimitId::MaxSessions
            })
        );
    }

    #[test]
    fn effective_usize_matches_the_advertised_value() {
        let limits =
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 3)]).expect("valid override");
        assert_eq!(limits.effective_usize(LimitId::MaxSessions), 3);
        assert_eq!(
            limits.effective_usize(LimitId::MaxContentBytesPerSession),
            268_435_456
        );
        assert_eq!(limits.default_value(LimitId::MaxSessions), 16);
        assert_eq!(limits.system_max(LimitId::MaxSessions), 64);
    }

    #[test]
    fn a_refusal_reports_the_enforced_value_and_actual_count() {
        let limits =
            Limits::new(&[LimitOverride::new(LimitId::MaxSessions, 4)]).expect("valid override");
        let refusal = limits.refusal(LimitId::MaxSessions, 5);
        assert_eq!(refusal.limit(), LimitId::MaxSessions);
        assert_eq!(refusal.effective(), 4);
        assert_eq!(refusal.actual(), 5);
        assert_eq!(refusal.to_string(), "limit max_sessions is 4, observed 5");
    }

    #[test]
    fn limits_error_display_names_the_limit_without_echoing_content() {
        let error = LimitsError::AboveSystemMax {
            limit: LimitId::MaxSessions,
            requested: 65,
            system_max: 64,
        };
        assert_eq!(
            error.to_string(),
            "limit max_sessions requested 65, above compiled maximum 64"
        );
        assert_eq!(
            LimitsError::FixedLimit {
                limit: LimitId::MaxFrameBytes,
                requested: 1,
                fixed: 262_144,
            }
            .to_string(),
            "limit max_frame_bytes is fixed at 262144, not 1"
        );
    }
}
