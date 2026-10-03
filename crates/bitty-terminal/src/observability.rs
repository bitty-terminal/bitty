//! Core observability boundary: retained mechanism vs optional policy.
//!
//! First staged slice of plan `W-100` (CarryCtx `CTX-0926`, issue `#1615`)
//! under the accepted `W-71` contract
//! (`bitty-docs/docs/development/observability-boundary.md`, status
//! accepted). This module **defines the explicit boundary**; it does not
//! retire any debug or trace code. Per the `W-71` removal gates, current
//! behavior stays in Core until `W-110`/`bitty-observability` shows the
//! conformance, parity, and evidence items — so every change in this slice
//! preserves behavior and documents the transition instead of changing it.
//!
//! # Retained in Core (always-on minimal mechanism, safe-mode clean)
//!
//! - The stderr verbosity gate ([`crate::logging`]): quiet [`crate::logging::LogLevel::Warn`]
//!   default; per-frame `bitty tick` lines require an explicit opt-in. The
//!   devtools trace path (`Runtime::tick` return plus inspect snapshots)
//!   keeps full fidelity regardless of this gate.
//! - The bounded, read-only inspect snapshots (`bitty-runtime` inspect plus
//!   the `bitty-ipc` devtools live store): `&self` publishing only, no
//!   socket, no thread, no Terminal Truth mutation.
//! - The authorization check itself (default-deny [`AuthorizationGate`]
//!   below), the redaction rules ([`redact_text_for_stderr`] for stderr,
//!   per-recipient payload redaction in `bitty-runtime`, opaque
//!   `secret://` handles plus scrubbers in `bitty-plugin-host`), and the
//!   bounds ([`bounds_hold`]).
//!
//! # Optional policy (explicit opt-in, default off, safe-mode clean)
//!
//! | Surface | Opt-in | Default | `--safe` |
//! | ------- | ------ | ------- | -------- |
//! | `bitty dev trace startup\|latency` | `dev-perf` cargo feature | off (exit 1 + [`crate::dev::TRACE_DISABLED_MESSAGE`]) | local-only, clean |
//! | `bitty dev capture\|synthesize\|dump\|overlay` | `dev-tools` cargo feature | off (exit 1 + [`crate::dev::TOOLS_DISABLED_MESSAGE`]) | local-only, clean |
//! | Per-frame `bitty tick` stderr lines | `--verbose` / `--log-level debug\|trace` / `BITTY_LOG` / `RUST_LOG` | off (quiet) | allowed (local diagnostics, never a trace artifact) |
//! | Synthetic demo pump | `BITTY_DEMO_PUMP=1` | off | suppressed ([`demo_pump_allowed`]) |
//! | Real-window first-frame marker | `BITTY_PERF_STARTUP_MARKER` env | off | never fabricates (headless guard) |
//! | `bitty-observability` implementation, exporters, metrics pipeline | not a Core dependency (placement parked) | off | not loaded |
//! | Input recording, clipboard / raw environment capture, trace files | separate opt-ins that do not exist in Core | off | off |
//!
//! # What this slice guarantees
//!
//! - No new external dependencies (only `std` plus the already-linked
//!   `bitty-plugin-host` scrubber; the `bitty-observability-api` shape is
//!   mirrored here without depending on it — dependency placement stays
//!   parked with the ADR-0004 owners per the `W-71` open points).
//! - No Event-Bus exposure: nothing here delivers, subscribes, or fans out
//!   plugin events; observers are readers, never authorities.
//! - No secret capture: observations carry counts, labels, and bounded
//!   synthetic text only; sensitive fields redact at emission (`P0-AC-026`)
//!   and `secret://` references stay log-safe handles.
//! - Version negotiation fails closed ([`ContractRange::intersect`]); before
//!   `0.1.0` the contract makes no stability claim (DIR-019).
//!
//! # Transition (no silent behavior change)
//!
//! - `bitty-runtime` `plugin_runtime::debug` (`DebugView`/`TraceHub`) is
//!   optional debug/trace *implementation* whose future owner is
//!   `bitty-observability`; it stays compiled in until the removal gates
//!   pass. `plugin_runtime::redaction` is retained permanently (Core owns
//!   redaction under `W-71`).
//! - The demo-pump safe-mode suppression logs one explicit warning instead
//!   of silently pumping; every other path keeps its exact prior behavior.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// Core-owned bounds (mirrored, not imported)
// ---------------------------------------------------------------------------

/// Maximum bytes of one observation kind or attribute key.
///
/// Mirrors the `bitty-observability-api` key bound without depending on it
/// (dependency placement is parked per the `W-71` open points); Core owns
/// the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_KEY_LEN: usize = 128;

/// Maximum attributes carried by one observation record.
///
/// Mirrors the `bitty-observability-api` attribute-count bound; Core owns
/// the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_ATTRS: usize = 64;

/// Maximum bytes of one textual attribute value.
///
/// Mirrors the `bitty-observability-api` text bound; Core owns the bound.
/// Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_ATTR_TEXT: usize = 4096;

/// Maximum encoded bytes of one observation record.
///
/// Mirrors the `bitty-observability-api` record bound; Core owns the bound.
/// Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_RECORD_BYTES: usize = 8192;

/// Maximum records in flight for one observer.
///
/// Overflow drops oldest first and the drop is explicit, never silent.
/// Mirrors the `bitty-observability-api` in-flight bound; Core owns the
/// bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_RECORDS_IN_FLIGHT: usize = 1024;

/// Total in-memory budget for buffered observation records.
///
/// A high-frequency producer cannot grow Core memory without limit.
/// Mirrors the `bitty-observability-api` total bound; Core owns the bound.
/// Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// Core-advertised observation contract major version.
///
/// `0` pre-`0.1.0`: the contract makes no stability claim (DIR-019), but
/// negotiation still fails closed via [`ContractRange::intersect`].
pub(crate) const OBS_CONTRACT_MAJOR: u16 = 0;

/// Core-advertised observation contract minor version (see [`OBS_CONTRACT_MAJOR`]).
pub(crate) const OBS_CONTRACT_MINOR: u16 = 0;

/// Shared greppable redaction token.
///
/// Same string as `bitty-plugin-host` `SECRET_REDACTED_MARKER` (parity is
/// pinned by test); production stderr scrubbing delegates to that reviewed
/// implementation, while [`redact_value_for_field`] applies the same token
/// to future observation-record fields (staged for `W-110`, see note there).
// CTX-0926 staged: only the field-level record path names this token in
// normal builds today; the stderr path delegates to the reviewed scrubber
// that carries its own copy. Kept (not inlined) so `W-110` record wiring
// has one Core-owned token to reference.
#[allow(dead_code)]
pub(crate) const REDACTED_MARKER: &str = "[redacted]";

/// Bounds sanity pin: relationships between the Core-owned bounds.
///
/// Referenced by `main` in a behavior-free `debug_assert!` so the bounds
/// stay pinned without changing any runtime path.
#[must_use]
pub(crate) fn bounds_hold() -> bool {
    MAX_OBSERVATION_KEY_LEN > 0
        && MAX_OBSERVATION_ATTRS > 0
        && MAX_OBSERVATION_ATTR_TEXT > 0
        && MAX_OBSERVATION_RECORD_BYTES >= MAX_OBSERVATION_ATTR_TEXT
        && MAX_OBSERVATION_RECORDS_IN_FLIGHT > 0
        && MAX_OBSERVATION_TOTAL_BYTES >= MAX_OBSERVATION_RECORD_BYTES
}

// ---------------------------------------------------------------------------
// Authorization gate (default-deny; Core owns the check itself)
// ---------------------------------------------------------------------------

/// Read-only observability capability.
///
/// Exactly the two DevTools-vocabulary scopes `debug.inspect` (structured
/// state) and `debug.trace` (record-stream subscription), granted
/// independently. There is deliberately no control scope: the seam is
/// read-only and control is not part of observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ObservabilityCapability {
    /// Inspect structured state (DevTools `debug.inspect`).
    DebugInspect,
    /// Subscribe to the bounded record stream (DevTools `debug.trace`).
    DebugTrace,
}

impl ObservabilityCapability {
    /// Capability identifier string (DevTools vocabulary, not a new token).
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DebugInspect => "debug.inspect",
            Self::DebugTrace => "debug.trace",
        }
    }

    /// Parses a capability identifier; `None` for anything else — including
    /// `debug.control`, which is not an observability capability.
    #[must_use]
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "debug.inspect" => Some(Self::DebugInspect),
            "debug.trace" => Some(Self::DebugTrace),
            _ => None,
        }
    }
}

/// Where an observer reaches Core from.
///
/// Every observer needs an explicit capability; an out-of-process observer
/// additionally needs explicit consent. Compilation, connection, or
/// configuration alone grants nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ObserverOrigin {
    /// A Core-owned subsystem observing its own mechanisms (in-memory,
    /// bounded; no external consent involved).
    InProcess,
    /// An observer outside the process (explicit consent required).
    OutOfProcess,
}

/// Why the authorization gate refused an observer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthorizationError {
    /// The required capability was not granted.
    MissingCapability(ObservabilityCapability),
    /// The observer is out-of-process and no explicit consent was recorded.
    ConsentRequired,
}

/// Default-deny authorization gate for the observation seam.
///
/// Starts closed: no capability granted, no consent recorded. Inspect and
/// trace are granted independently; there is no control grant to give.
// The grant/consent mutation methods are staged for `W-110` wiring: the
// default build never grants (pinned by `default_external_trace_attached`
// plus tests), so normal builds name no caller for them yet.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuthorizationGate {
    granted: BTreeSet<ObservabilityCapability>,
    consented: bool,
}

impl AuthorizationGate {
    /// Closed gate: no grants, no consent (the default-deny state).
    #[must_use]
    pub(crate) fn default_deny() -> Self {
        Self::default()
    }

    /// Grants one capability scope; other scopes stay ungranted.
    // CTX-0926 staged for `W-110` grant plumbing.
    #[allow(dead_code)]
    pub(crate) fn grant(&mut self, capability: ObservabilityCapability) {
        self.granted.insert(capability);
    }

    /// Revokes one capability scope.
    // CTX-0926 staged for `W-110` grant plumbing.
    #[allow(dead_code)]
    pub(crate) fn revoke(&mut self, capability: ObservabilityCapability) {
        self.granted.remove(&capability);
    }

    /// Whether `capability` was explicitly granted.
    // CTX-0926 staged for `W-110` grant plumbing.
    #[allow(dead_code)]
    pub(crate) fn is_granted(&self, capability: ObservabilityCapability) -> bool {
        self.granted.contains(&capability)
    }

    /// Records explicit consent for out-of-process observers.
    // CTX-0926 staged for `W-110` grant plumbing.
    #[allow(dead_code)]
    pub(crate) fn grant_consent(&mut self) {
        self.consented = true;
    }

    /// Whether explicit out-of-process consent is recorded.
    // CTX-0926 staged for `W-110` grant plumbing.
    #[allow(dead_code)]
    pub(crate) fn consented(&self) -> bool {
        self.consented
    }

    /// Default-deny check: the capability must be granted, and an
    /// out-of-process observer additionally needs recorded consent.
    pub(crate) fn check(
        &self,
        capability: ObservabilityCapability,
        origin: ObserverOrigin,
    ) -> Result<(), AuthorizationError> {
        if !self.granted.contains(&capability) {
            return Err(AuthorizationError::MissingCapability(capability));
        }
        if origin == ObserverOrigin::OutOfProcess && !self.consented {
            return Err(AuthorizationError::ConsentRequired);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Contract version negotiation (fail-closed)
// ---------------------------------------------------------------------------

/// One observation-contract version (product versioning is separate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ContractVersion {
    /// Major version: renaming or changing a record's meaning advances it.
    pub major: u16,
    /// Minor version: an ignorable additive record/attribute advances it.
    pub minor: u16,
}

impl ContractVersion {
    /// Builds a contract version.
    #[must_use]
    pub(crate) const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

/// Inclusive contract version range (advertised by Core, declared by the observer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContractRange {
    /// Lowest compatible version.
    pub min: ContractVersion,
    /// Highest compatible version.
    pub max: ContractVersion,
}

impl ContractRange {
    /// Builds an inclusive range.
    #[must_use]
    pub(crate) const fn new(min: ContractVersion, max: ContractVersion) -> Self {
        Self { min, max }
    }

    /// Intersection of two ranges, or `None` when they do not overlap.
    ///
    /// Core attaches an observer only on a non-empty intersection and
    /// refuses otherwise — never best-effort on mismatch.
    #[must_use]
    pub(crate) fn intersect(&self, other: &Self) -> Option<Self> {
        let min = self.min.max(other.min);
        let max = self.max.min(other.max);
        if min <= max {
            Some(Self { min, max })
        } else {
            None
        }
    }
}

/// Core-advertised contract range (today the single pre-`0.1.0` point).
#[must_use]
pub(crate) fn core_contract_range() -> ContractRange {
    let point = ContractVersion::new(OBS_CONTRACT_MAJOR, OBS_CONTRACT_MINOR);
    ContractRange::new(point, point)
}

/// Fail-closed attach decision for one external observer.
///
/// Safe mode denies everything external. Otherwise capability, consent
/// (for out-of-process origins), and a non-empty version intersection must
/// all hold; any single failure refuses the attach.
#[must_use]
pub(crate) fn can_attach_external_observer(
    safe: bool,
    gate: &AuthorizationGate,
    capability: ObservabilityCapability,
    origin: ObserverOrigin,
    observer_range: &ContractRange,
) -> bool {
    if safe {
        return false;
    }
    if gate.check(capability, origin).is_err() {
        return false;
    }
    core_contract_range().intersect(observer_range).is_some()
}

/// Default-build pin: a default-deny gate with no consent never attaches an
/// out-of-process trace observer, in either mode.
///
/// Called from `main` in a behavior-free `debug_assert!` so the posture
/// stays pinned without changing any runtime path.
#[must_use]
pub(crate) fn default_external_trace_attached(safe: bool) -> bool {
    let gate = AuthorizationGate::default_deny();
    can_attach_external_observer(
        safe,
        &gate,
        ObservabilityCapability::DebugTrace,
        ObserverOrigin::OutOfProcess,
        &core_contract_range(),
    )
}

/// Default-build pin for the in-process origin: even inside the process, an
/// attach through the seam needs an explicit grant (Core's own internal
/// in-memory observation does not go through attach at all).
///
/// Called from `main` in a behavior-free `debug_assert!` (see
/// [`default_external_trace_attached`]).
#[must_use]
pub(crate) fn default_in_process_trace_attached() -> bool {
    let gate = AuthorizationGate::default_deny();
    can_attach_external_observer(
        false,
        &gate,
        ObservabilityCapability::DebugTrace,
        ObserverOrigin::InProcess,
        &core_contract_range(),
    )
}

/// Debug-only invariant pins for the default posture (called from `main`).
///
/// Bundles every executable pin — bounds, both default-deny origins, and
/// the capability vocabulary (including "no control scope") — so the
/// boundary stays executable, not just documentary. Not a runtime check:
/// release builds erase the call site.
#[must_use]
pub(crate) fn default_posture_pins_hold(safe: bool) -> bool {
    bounds_hold()
        && !default_external_trace_attached(safe)
        && !default_in_process_trace_attached()
        && ObservabilityCapability::parse("debug.control").is_none()
        && ObservabilityCapability::DebugTrace.as_str() == "debug.trace"
        && ObservabilityCapability::DebugInspect.as_str() == "debug.inspect"
}

// ---------------------------------------------------------------------------
// Redaction at emission (P0-AC-026)
// ---------------------------------------------------------------------------

/// Core-owned sensitive-field classifier (fail-closed over-redact).
///
/// A newly added attribute is sensitive by default until classified: any
/// field name containing one of these needles redacts at emission. Kept
/// deliberately narrow (never bare `key`, so `keymaps` and tick counters
/// stay readable) while covering credential-shaped names.
// CTX-0926 staged: the production stderr path delegates key/value scrubbing
// to the reviewed `bitty-plugin-host` implementation (same rule, no fork);
// this classifier pins the Core-owned rule for the `W-110` record fields.
#[allow(dead_code)]
#[must_use]
pub(crate) fn is_sensitive_field_name(name: &str) -> bool {
    // MSRV 1.85: no `to_lowercase` short-circuit tricks; a small fixed
    // needle table over one lowered copy is O(n) and total.
    const NEEDLES: &[&str] = &[
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "private",
        "bearer",
        "cookie",
        "session",
        "auth",
        "api_key",
        "apikey",
        "api-key",
    ];
    let lowered = name.to_lowercase();
    NEEDLES.iter().any(|needle| lowered.contains(needle))
}

/// Truncates text to at most `max_bytes` on a character boundary.
///
/// Returns the (possibly truncated) text plus whether truncation happened,
/// so a drop is explicit, never silent.
// CTX-0926 staged for the `W-110` record encoder; the stderr path below
// never truncates today (diagnostics are short by construction).
#[allow(dead_code)]
#[must_use]
pub(crate) fn bound_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Field-level redaction-at-emission for observation-record values.
///
/// Sensitive-typed fields become [`REDACTED_MARKER`]; the raw value never
/// reaches an observer, a buffer, or a file. Non-sensitive values are
/// bounded to [`MAX_OBSERVATION_ATTR_TEXT`] with an explicit truncation
/// flag folded into the marker form (a truncated value reports redacted —
/// over-redact rather than leak a half-cut secret).
// CTX-0926 staged for `W-110`: no record producer exists in Core yet, so
// normal builds name no caller. Pinned and tested here so the record path
// cannot land without the emission-time rule.
#[allow(dead_code)]
#[must_use]
pub(crate) fn redact_value_for_field<'a>(field: &str, value: &'a str) -> Cow<'a, str> {
    if is_sensitive_field_name(field) {
        return Cow::Borrowed(REDACTED_MARKER);
    }
    let (bounded, truncated) = bound_text(value, MAX_OBSERVATION_ATTR_TEXT);
    if truncated {
        Cow::Borrowed(REDACTED_MARKER)
    } else if bounded == value {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(bounded)
    }
}

/// Production redaction-at-emission for stderr diagnostics.
///
/// Every `logging::info`/`warn` line crosses the Core-to-stderr observer
/// boundary here before `eprintln!`: the reviewed
/// `bitty-plugin-host` scrubber replaces sensitive-key values and
/// secret-shaped tokens with `[redacted]` while preserving log-safe
/// `secret://` handle references. Legit diagnostics (counts, labels,
/// paths, tick lines) pass through byte-identical, so wiring this in
/// changes no existing output — it only closes the leak path for text
/// that should never have reached diagnostics in the first place.
#[must_use]
pub(crate) fn redact_text_for_stderr(text: &str) -> Cow<'_, str> {
    let scrubbed = bitty_plugin_host::secrets::scrub_text_with_secrets(text, &[]);
    if scrubbed == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(scrubbed)
    }
}

// ---------------------------------------------------------------------------
// Safe-mode-clean optional policy
// ---------------------------------------------------------------------------

/// Safe-mode-clean demo-pump decision.
///
/// The synthetic input pump is opt-in debug policy (`BITTY_DEMO_PUMP=1`,
/// default off) and must never attach in `--safe` recovery, where startup
/// shows only the real shell. Non-safe behavior is unchanged.
#[must_use]
pub(crate) fn demo_pump_allowed(safe: bool, env_enabled: bool) -> bool {
    env_enabled && !safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_hold_and_mirror_api_defaults() {
        assert!(bounds_hold());
        // Mirror values stay aligned with the `bitty-observability-api`
        // defaults they shadow (no dependency by design, see module docs).
        assert_eq!(MAX_OBSERVATION_RECORD_BYTES, 8192);
        assert_eq!(MAX_OBSERVATION_RECORDS_IN_FLIGHT, 1024);
        assert_eq!(MAX_OBSERVATION_TOTAL_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_OBSERVATION_ATTR_TEXT, 4096);
        assert_eq!(REDACTED_MARKER, "[redacted]");
        assert_eq!(
            REDACTED_MARKER,
            bitty_plugin_host::secrets::SECRET_REDACTED_MARKER
        );
    }

    #[test]
    fn gate_starts_default_deny() {
        let gate = AuthorizationGate::default_deny();
        assert_eq!(
            gate.check(
                ObservabilityCapability::DebugInspect,
                ObserverOrigin::InProcess
            ),
            Err(AuthorizationError::MissingCapability(
                ObservabilityCapability::DebugInspect
            ))
        );
        assert_eq!(
            gate.check(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess
            ),
            Err(AuthorizationError::MissingCapability(
                ObservabilityCapability::DebugTrace
            ))
        );
    }

    #[test]
    fn inspect_and_trace_are_separately_granted() {
        let mut gate = AuthorizationGate::default_deny();
        gate.grant(ObservabilityCapability::DebugInspect);
        assert!(gate.is_granted(ObservabilityCapability::DebugInspect));
        assert!(!gate.is_granted(ObservabilityCapability::DebugTrace));
        assert!(
            gate.check(
                ObservabilityCapability::DebugInspect,
                ObserverOrigin::InProcess
            )
            .is_ok()
        );
        assert_eq!(
            gate.check(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::InProcess
            ),
            Err(AuthorizationError::MissingCapability(
                ObservabilityCapability::DebugTrace
            ))
        );
        gate.revoke(ObservabilityCapability::DebugInspect);
        assert!(!gate.is_granted(ObservabilityCapability::DebugInspect));
    }

    #[test]
    fn out_of_process_needs_consent() {
        let mut gate = AuthorizationGate::default_deny();
        gate.grant(ObservabilityCapability::DebugTrace);
        assert!(!gate.consented());
        assert_eq!(
            gate.check(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess
            ),
            Err(AuthorizationError::ConsentRequired)
        );
        // In-process observation with the same grant needs no consent.
        assert!(
            gate.check(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::InProcess
            )
            .is_ok()
        );
        gate.grant_consent();
        assert!(gate.consented());
        assert!(
            gate.check(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess
            )
            .is_ok()
        );
    }

    #[test]
    fn no_control_scope_exists() {
        assert_eq!(
            ObservabilityCapability::parse("debug.inspect"),
            Some(ObservabilityCapability::DebugInspect)
        );
        assert_eq!(
            ObservabilityCapability::parse("debug.trace"),
            Some(ObservabilityCapability::DebugTrace)
        );
        assert_eq!(ObservabilityCapability::parse("debug.control"), None);
        assert_eq!(ObservabilityCapability::parse("trace"), None);
        assert_eq!(ObservabilityCapability::parse(""), None);
        assert_eq!(
            ObservabilityCapability::DebugInspect.as_str(),
            "debug.inspect"
        );
        assert_eq!(ObservabilityCapability::DebugTrace.as_str(), "debug.trace");
    }

    #[test]
    fn version_negotiation_fails_closed() {
        let core = core_contract_range();
        assert!(core.intersect(&core).is_some());
        let newer = ContractRange::new(
            ContractVersion::new(OBS_CONTRACT_MAJOR + 1, 0),
            ContractVersion::new(OBS_CONTRACT_MAJOR + 1, 0),
        );
        assert_eq!(core.intersect(&newer), None);
        // Adjacent overlap still attaches on the intersection.
        let overlapping = ContractRange::new(
            ContractVersion::new(OBS_CONTRACT_MAJOR, 0),
            ContractVersion::new(OBS_CONTRACT_MAJOR + 1, 0),
        );
        assert_eq!(core.intersect(&overlapping), Some(core));
    }

    #[test]
    fn default_build_attaches_no_external_trace() {
        assert!(!default_external_trace_attached(false));
        assert!(!default_external_trace_attached(true));
        // Even a fully granted gate stays denied under `--safe`.
        let mut gate = AuthorizationGate::default_deny();
        gate.grant(ObservabilityCapability::DebugTrace);
        gate.grant_consent();
        assert!(!can_attach_external_observer(
            true,
            &gate,
            ObservabilityCapability::DebugTrace,
            ObserverOrigin::OutOfProcess,
            &core_contract_range()
        ));
        assert!(can_attach_external_observer(
            false,
            &gate,
            ObservabilityCapability::DebugTrace,
            ObserverOrigin::OutOfProcess,
            &core_contract_range()
        ));
    }

    #[test]
    fn sensitive_fields_redact_at_emission() {
        for field in [
            "password",
            "api_token",
            "AWS_SECRET",
            "Authorization",
            "session_key",
            "cookie",
            "private_key",
            "client_secret",
        ] {
            assert!(is_sensitive_field_name(field), "{field}");
            assert_eq!(
                redact_value_for_field(field, "hunter2"),
                Cow::Borrowed("[redacted]")
            );
        }
        // Non-sensitive fields pass through byte-identical.
        for field in ["theme", "keymaps", "frame", "fills", "layout", "gen"] {
            assert!(!is_sensitive_field_name(field), "{field}");
            assert_eq!(
                redact_value_for_field(field, "default"),
                Cow::Borrowed("default")
            );
        }
        // New attributes are sensitive by default until classified: an
        // unknown compound containing a needle still redacts (over-redact,
        // never leak).
        assert_eq!(
            redact_value_for_field("mystery_password_hash", "hunter2"),
            Cow::Borrowed("[redacted]")
        );
    }

    #[test]
    fn seeded_secrets_never_reach_records() {
        let seeded = "seeded-secret-value-hunter2";
        assert_eq!(
            redact_value_for_field("password", seeded),
            Cow::Borrowed("[redacted]")
        );
        // The stderr scrubber removes the seeded value even when it arrives
        // as free text (known-value corpus), while `secret://` handles —
        // the log-safe reference form — are preserved.
        let scrubbed = bitty_plugin_host::secrets::scrub_text_with_secrets(
            &format!("login with {seeded} done"),
            &[seeded],
        );
        assert!(!scrubbed.contains(seeded), "{scrubbed}");
        let handled: Cow<'_, str> = redact_text_for_stderr("credential = secret://db-password");
        assert!(handled.contains("secret://db-password"), "{handled}");
        // Legit diagnostics pass through byte-identical (no behavior change).
        for line in [
            "bitty tick: frame=1 fills=7 glyphs=3 headless=true gen=9 presented_frames=1 focused=None leafs=1 gpu=false crossfont=false images=0 images_skipped=0",
            "bitty: theme 'default' via default",
            "bitty: keymaps resolved (3 entries)",
            "bitty: layout installed — leafs=1",
        ] {
            assert_eq!(redact_text_for_stderr(line), Cow::Borrowed(line), "{line}");
        }
    }

    #[test]
    fn truncation_is_explicit_never_silent() {
        let (short, truncated) = bound_text("abc", 64);
        assert_eq!(short, "abc");
        assert!(!truncated);
        let (cut, truncated) = bound_text("abcdef", 4);
        assert_eq!(cut, "abcd");
        assert!(truncated);
        // Multi-byte characters never split: the cut lands on a boundary.
        let (cut, truncated) = bound_text("aébc", 2);
        assert_eq!(cut, "a");
        assert!(truncated);
        // Oversize non-sensitive values over-redact rather than leak a cut fragment.
        let big = "x".repeat(MAX_OBSERVATION_ATTR_TEXT + 1);
        assert_eq!(
            redact_value_for_field("note", &big),
            Cow::Borrowed("[redacted]")
        );
    }

    #[test]
    fn demo_pump_is_opt_in_and_safe_mode_clean() {
        assert!(!demo_pump_allowed(false, false));
        assert!(demo_pump_allowed(false, true));
        assert!(!demo_pump_allowed(true, false));
        assert!(!demo_pump_allowed(true, true));
    }
}
