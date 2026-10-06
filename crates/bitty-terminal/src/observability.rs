//! Core observability boundary: retained mechanism over the canonical API.
//!
//! Second slice of plan `W-110` (CarryCtx `CTX-0981`, issue `#1649`)
//! under the accepted `W-71` contract
//! (`bitty-docs/docs/development/observability-boundary.md`, status
//! accepted). This module is the thin Core-owned verification over the
//! canonical `bitty-observability-api` contract: the transitional mirrors
//! staged in `CTX-0926` (local copies of the capability, gate, version, and
//! bound definitions, plus the staged field-level record path) are deleted
//! here, and the canonical zero-dependency types are imported instead. Per
//! the `W-71` removal gates the record-level implementation (buffers,
//! filters, emission-time field redaction) lives in `bitty-observability`
//! and is tested there; Core keeps no record producer.
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
//!   verification below), the redaction rules ([`redact_text_for_stderr`] for stderr,
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
//! | `bitty-observability` implementation, exporters, metrics pipeline | canonical API linked by the composition root only (zero dependencies) | off | not loaded |
//! | Input recording, clipboard / raw environment capture, trace files | separate opt-ins that do not exist in Core | off | off |
//!
//! # What this slice guarantees
//!
//! - One canonical contract: the bound, capability, gate, and version types
//!   are imported from `bitty-observability-api` (linked ONLY by the
//!   `bitty-terminal` composition root), never redefined here. The
//!   Core-owned aliases below pin the exact values, so an upstream drift
//!   fails tests instead of changing behavior silently.
//! - No new runtime weight: the API crate has zero dependencies, no network,
//!   and no filesystem access, so default builds link no exporters, metrics
//!   pipeline, or tracing runtime.
//! - No Event-Bus exposure: nothing here delivers, subscribes, or fans out
//!   plugin events; observers are readers, never authorities.
//! - No secret capture: stderr diagnostics redact at emission (`P0-AC-026`)
//!   and `secret://` references stay log-safe handles. Field-level record
//!   redaction is implemented and tested in `bitty-observability-core`
//!   (`redact_observation` / `redact_payload`); Core has no record producer
//!   and keeps none.
//! - Version negotiation fails closed ([`ContractRange::intersect`], via the
//!   canonical crate); before `0.1.0` the contract makes no stability claim
//!   (DIR-019).
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

// ---------------------------------------------------------------------------
// Canonical contract types (W-110): imported, never redefined.
// ---------------------------------------------------------------------------
//
// CTX-0981 deletes the CTX-0926 mirrors: the bound, capability, gate, and
// version definitions now come from the canonical `bitty-observability-api`
// crate (linked ONLY by the `bitty-terminal` composition root). Core owns
// the verification over these types below, not the type definitions.

/// Canonical observation-seam contract types.
///
/// Re-exported `pub(crate)` so Core verification and its pins name the exact
/// types the extension implementation honors. See the `W-71` boundary for
/// ownership: Core owns the check, the emission-time redaction, the bounds,
/// and the version range check; the record implementation lives outside.
pub(crate) use bitty_observability_api::{
    AuthorizationGate, ContractRange, ContractVersion, ObservabilityCapability, ObserverOrigin,
};

// ---------------------------------------------------------------------------
// Core-owned bounds (aliases over the canonical values, pinned by tests)
// ---------------------------------------------------------------------------

/// Maximum bytes of one observation kind or attribute key.
///
/// Alias of the canonical `bitty-observability-api` bound; Core owns the
/// bound and pins the literal value in tests. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_KEY_LEN: usize = bitty_observability_api::event::MAX_KEY_LEN;

/// Maximum attributes carried by one observation record.
///
/// Alias of the canonical `bitty-observability-api` bound; Core owns the
/// bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_ATTRS: usize = bitty_observability_api::event::MAX_ATTRIBUTES;

/// Maximum bytes of one textual attribute value.
///
/// Alias of the canonical `bitty-observability-api` text bound; Core owns
/// the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_ATTR_TEXT: usize =
    bitty_observability_api::event::MAX_ATTRIBUTE_TEXT_LEN;

/// Maximum encoded bytes of one observation record.
///
/// Alias of the canonical `bitty-observability-api` record bound; Core owns
/// the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_RECORD_BYTES: usize =
    bitty_observability_api::DEFAULT_MAX_RECORD_BYTES;

/// Maximum records in flight for one observer.
///
/// Overflow drops oldest first and the drop is explicit, never silent.
/// Alias of the canonical `bitty-observability-api` in-flight bound; Core
/// owns the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_RECORDS_IN_FLIGHT: usize =
    bitty_observability_api::DEFAULT_MAX_RECORDS;

/// Total in-memory budget for buffered observation records.
///
/// A high-frequency producer cannot grow Core memory without limit.
/// Alias of the canonical `bitty-observability-api` total bound; Core owns
/// the bound. Pinned by [`bounds_hold`].
pub(crate) const MAX_OBSERVATION_TOTAL_BYTES: usize =
    bitty_observability_api::DEFAULT_MAX_TOTAL_BYTES;

/// Core-advertised observation contract major version.
///
/// `0` pre-`0.1.0`: the contract makes no stability claim (DIR-019), but
/// negotiation still fails closed via [`ContractRange::intersect`].
pub(crate) const OBS_CONTRACT_MAJOR: u16 = 0;

/// Core-advertised observation contract minor version (see [`OBS_CONTRACT_MAJOR`]).
pub(crate) const OBS_CONTRACT_MINOR: u16 = 0;

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
// Core-owned verification (default-deny check plus fail-closed negotiation)
// ---------------------------------------------------------------------------

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
    if gate.authorize(capability, origin).is_err() {
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
// Redaction at emission (P0-AC-026): the retained Core emission path
// ---------------------------------------------------------------------------

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
///
/// Field-level record redaction (for observation records, of which Core
/// keeps no producer) is implemented and tested in
/// `bitty-observability-core` (`redact_observation` / `redact_payload`).
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
    use bitty_observability_api::AuthorizationError;

    #[test]
    fn bounds_hold_and_match_canonical_api() {
        assert!(bounds_hold());
        // Literal pins: an upstream drift breaks here instead of changing
        // Core behavior silently.
        assert_eq!(MAX_OBSERVATION_KEY_LEN, 128);
        assert_eq!(MAX_OBSERVATION_ATTRS, 64);
        assert_eq!(MAX_OBSERVATION_ATTR_TEXT, 4096);
        assert_eq!(MAX_OBSERVATION_RECORD_BYTES, 8192);
        assert_eq!(MAX_OBSERVATION_RECORDS_IN_FLIGHT, 1024);
        assert_eq!(MAX_OBSERVATION_TOTAL_BYTES, 4 * 1024 * 1024);
        // Canonical parity: the Core aliases name the exact API bounds.
        assert_eq!(
            MAX_OBSERVATION_KEY_LEN,
            bitty_observability_api::event::MAX_KEY_LEN
        );
        assert_eq!(
            MAX_OBSERVATION_ATTRS,
            bitty_observability_api::event::MAX_ATTRIBUTES
        );
        assert_eq!(
            MAX_OBSERVATION_ATTR_TEXT,
            bitty_observability_api::event::MAX_ATTRIBUTE_TEXT_LEN
        );
        assert_eq!(
            MAX_OBSERVATION_RECORD_BYTES,
            bitty_observability_api::DEFAULT_MAX_RECORD_BYTES
        );
        assert_eq!(
            MAX_OBSERVATION_RECORDS_IN_FLIGHT,
            bitty_observability_api::DEFAULT_MAX_RECORDS
        );
        assert_eq!(
            MAX_OBSERVATION_TOTAL_BYTES,
            bitty_observability_api::DEFAULT_MAX_TOTAL_BYTES
        );
        // The shared greppable redaction token stays `[redacted]`: the
        // production stderr scrubber carries the reviewed copy.
        assert_eq!(
            bitty_plugin_host::secrets::SECRET_REDACTED_MARKER,
            "[redacted]"
        );
    }

    #[test]
    fn gate_starts_default_deny() {
        let gate = AuthorizationGate::default_deny();
        assert_eq!(
            gate.authorize(
                ObservabilityCapability::DebugInspect,
                ObserverOrigin::InProcess
            ),
            Err(AuthorizationError::MissingCapability(
                ObservabilityCapability::DebugInspect
            ))
        );
        assert_eq!(
            gate.authorize(
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
            gate.authorize(
                ObservabilityCapability::DebugInspect,
                ObserverOrigin::InProcess
            )
            .is_ok()
        );
        assert_eq!(
            gate.authorize(
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
            gate.authorize(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess
            ),
            Err(AuthorizationError::ConsentRequired)
        );
        // In-process observation with the same grant needs no consent.
        assert!(
            gate.authorize(
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::InProcess
            )
            .is_ok()
        );
        gate.grant_consent();
        assert!(gate.consented());
        assert!(
            gate.authorize(
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
    fn canonical_subscribe_agrees_with_core_verification() {
        use bitty_observability_api::{SubscriptionError, subscribe};

        // A fully granted plus consented gate with an intersecting range
        // attaches through the canonical path exactly when Core verification
        // allows it.
        let mut gate = AuthorizationGate::default_deny();
        gate.grant(ObservabilityCapability::DebugTrace);
        gate.grant_consent();
        let observer = core_contract_range();
        assert!(can_attach_external_observer(
            false,
            &gate,
            ObservabilityCapability::DebugTrace,
            ObserverOrigin::OutOfProcess,
            &observer,
        ));
        assert!(
            subscribe(
                &gate,
                1,
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess,
                observer,
                core_contract_range(),
                MAX_OBSERVATION_RECORDS_IN_FLIGHT,
            )
            .is_ok()
        );
        // An incompatible range fails closed through both paths.
        let newer = ContractRange::new(
            ContractVersion::new(OBS_CONTRACT_MAJOR + 1, 0),
            ContractVersion::new(OBS_CONTRACT_MAJOR + 1, 0),
        );
        assert!(!can_attach_external_observer(
            false,
            &gate,
            ObservabilityCapability::DebugTrace,
            ObserverOrigin::OutOfProcess,
            &newer,
        ));
        assert_eq!(
            subscribe(
                &gate,
                1,
                ObservabilityCapability::DebugTrace,
                ObserverOrigin::OutOfProcess,
                newer,
                core_contract_range(),
                MAX_OBSERVATION_RECORDS_IN_FLIGHT,
            ),
            Err(SubscriptionError::IncompatibleVersion)
        );
    }

    #[test]
    fn seeded_secrets_never_reach_stderr() {
        use std::borrow::Cow;

        let seeded = "seeded-secret-value-hunter2";
        // The stderr scrubber removes the seeded value even when it arrives
        // as free text (known-value corpus), while `secret://` handles —
        // the log-safe reference form — are preserved.
        let scrubbed = bitty_plugin_host::secrets::scrub_text_with_secrets(
            &format!("login with {seeded} done"),
            &[seeded],
        );
        assert!(
            !scrubbed.contains(seeded),
            "scrubbed text still contains the seeded secret"
        );
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
    fn demo_pump_is_opt_in_and_safe_mode_clean() {
        assert!(!demo_pump_allowed(false, false));
        assert!(demo_pump_allowed(false, true));
        assert!(!demo_pump_allowed(true, false));
        assert!(!demo_pump_allowed(true, true));
    }
}
