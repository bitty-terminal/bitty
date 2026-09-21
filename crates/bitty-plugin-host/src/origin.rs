//! Unknown-origin restrictive policy (R-020, P0-AC-032).
//!
//! Session-origin detection is advisory only and must never be the sole
//! security boundary (threat model, boundary map). When detection is wrong
//! or unavailable, the `Unknown` origin uses the restrictive policy, and
//! only an explicit user override relaxes it.
//!
//! This module is pure data + classification: it never sniffs the
//! environment itself. The caller supplies advisory evidence (for example
//! the `bitty-pty` `is_remote` hint or session env signals) as
//! [`OriginSignals`]; classification fails closed to [`DetectedOrigin::Unknown`]
//! on absent or conflicting signals so spoofed evidence cannot smuggle in
//! the permissive local policy. Policy selection via
//! [`resolve_origin_policy`] then maps `Unknown` (and `Remote`) to
//! [`OriginPolicy::Restrictive`] unless the caller passes the explicit
//! [`OriginOverride::RelaxToStandard`] opt-in, which must originate from
//! explicit user action (for example a config `env.allowlist` path gate),
//! never from detection.
//!
//! # Ownership & constraints
//!
//! - Pure data + validation: no file I/O, no network, no env reads, no code
//!   execution. Headlessly testable on Linux CI and Windows.
//! - `#![forbid(unsafe_code)]` at the crate level, `MSRV 1.85`, `edition = "2024"`.
//! - Detection output is advisory: callers must still enforce their own
//!   boundaries (capability grants, consent gates); this policy only selects
//!   how restrictive the surrounding defaults are.

use std::fmt;

/// Advisory evidence about session origin, supplied by the caller.
///
/// Each flag is a hint, not a fact: either flag may be spoofed, absent, or
/// stale. Conflicting or absent evidence classifies as
/// [`DetectedOrigin::Unknown`] (fail closed); see [`classify_origin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OriginSignals {
    /// Caller observed remote-session evidence (for example the `bitty-pty`
    /// `is_remote` hint or a remote env marker such as `SSH_CONNECTION`).
    pub remote_hint: bool,
    /// Caller observed local-session evidence (for example a local display
    /// session marker with no remote indicators).
    pub local_hint: bool,
}

impl OriginSignals {
    /// Evidence carrying no signal either way; classifies as `Unknown`.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            remote_hint: false,
            local_hint: false,
        }
    }

    /// Remote-only evidence; classifies as `Remote` unless spoofed by a
    /// co-present local hint (see [`classify_origin`]).
    #[must_use]
    pub const fn remote_only() -> Self {
        Self {
            remote_hint: true,
            local_hint: false,
        }
    }

    /// Local-only evidence; classifies as `Local` unless spoofed by a
    /// co-present remote hint (see [`classify_origin`]).
    #[must_use]
    pub const fn local_only() -> Self {
        Self {
            remote_hint: false,
            local_hint: true,
        }
    }

    /// Conflicting evidence (both hints present, as under signal spoofing);
    /// classifies as `Unknown`.
    #[must_use]
    pub const fn conflicting() -> Self {
        Self {
            remote_hint: true,
            local_hint: true,
        }
    }
}

/// Advisory session-origin classification.
///
/// Advisory only: never the sole security boundary. Callers combine this
/// with their own enforcement (capability grants, consent gates) and select
/// defaults via [`resolve_origin_policy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectedOrigin {
    /// Local-only evidence with no remote indicators.
    Local,
    /// Remote-only evidence with no local indicators.
    Remote,
    /// Detection unavailable, absent, or conflicting (spoofed). Uses the
    /// restrictive policy per R-020.
    Unknown,
}

impl fmt::Display for DetectedOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Unknown => "unknown",
        };
        f.write_str(label)
    }
}

/// Classify caller-supplied advisory evidence, fail-closed.
///
/// - Local hint alone yields [`DetectedOrigin::Local`].
/// - Remote hint alone yields [`DetectedOrigin::Remote`].
/// - No signal, or both signals at once (conflicting / spoofed evidence),
///   yields [`DetectedOrigin::Unknown`], which resolves to the restrictive
///   policy. Forced misclassification therefore always falls back to
///   restrictive, never to the permissive local policy.
#[must_use]
pub const fn classify_origin(signals: OriginSignals) -> DetectedOrigin {
    match (signals.local_hint, signals.remote_hint) {
        (true, false) => DetectedOrigin::Local,
        (false, true) => DetectedOrigin::Remote,
        _ => DetectedOrigin::Unknown,
    }
}

/// Policy strictness selected for a session origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OriginPolicy {
    /// Standard local defaults for positively detected local sessions
    /// (or explicit user override).
    Standard,
    /// Restrictive defaults for `Remote` and `Unknown` origins: consent and
    /// grant gates that are lenient under the standard policy require
    /// explicit approval here.
    Restrictive,
}

impl OriginPolicy {
    /// True for [`OriginPolicy::Restrictive`].
    #[must_use]
    pub const fn is_restrictive(self) -> bool {
        matches!(self, Self::Restrictive)
    }
}

impl fmt::Display for OriginPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Standard => "standard",
            Self::Restrictive => "restrictive",
        };
        f.write_str(label)
    }
}

/// Explicit user override for the origin policy.
///
/// The only path that relaxes a non-local detection to the standard policy.
/// Must originate from explicit user action (for example a config
/// `env.allowlist` path gate with recorded consent), never from detection
/// output. There is no implicit or detection-driven relaxation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OriginOverride {
    /// No override: detection alone decides; `Remote` and `Unknown` stay
    /// restrictive.
    #[default]
    None,
    /// Explicit user opt-in to apply the standard local policy despite a
    /// non-local detection.
    RelaxToStandard,
}

/// Select the session policy for an advisory detection plus override.
///
/// - [`OriginOverride::RelaxToStandard`] yields [`OriginPolicy::Standard`]
///   for any detection: the single explicit escape hatch.
/// - Otherwise [`DetectedOrigin::Local`] yields `Standard`, while
///   [`DetectedOrigin::Remote`] and [`DetectedOrigin::Unknown`] yield
///   [`OriginPolicy::Restrictive`]. Detection is advisory and can only
///   restrict, never relax.
#[must_use]
pub const fn resolve_origin_policy(
    detected: DetectedOrigin,
    origin_override: OriginOverride,
) -> OriginPolicy {
    match origin_override {
        OriginOverride::RelaxToStandard => OriginPolicy::Standard,
        OriginOverride::None => match detected {
            DetectedOrigin::Local => OriginPolicy::Standard,
            DetectedOrigin::Remote | DetectedOrigin::Unknown => OriginPolicy::Restrictive,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_single_hints_classify() {
        assert_eq!(
            classify_origin(OriginSignals::local_only()),
            DetectedOrigin::Local
        );
        assert_eq!(
            classify_origin(OriginSignals::remote_only()),
            DetectedOrigin::Remote
        );
    }

    #[test]
    fn advisory_absent_evidence_is_unknown() {
        assert_eq!(
            classify_origin(OriginSignals::none()),
            DetectedOrigin::Unknown
        );
        assert_eq!(
            classify_origin(OriginSignals {
                remote_hint: false,
                local_hint: false,
            }),
            DetectedOrigin::Unknown
        );
    }

    #[test]
    fn advisory_conflicting_evidence_is_unknown() {
        // Spoofed signals (both hints present, e.g. forged local marker on a
        // remote session, or tampered SSH_CONNECTION mimicry) must not
        // resolve to either concrete origin.
        assert_eq!(
            classify_origin(OriginSignals::conflicting()),
            DetectedOrigin::Unknown
        );
        assert_eq!(
            classify_origin(OriginSignals {
                remote_hint: true,
                local_hint: true,
            }),
            DetectedOrigin::Unknown
        );
    }

    #[test]
    fn restrictive_unknown_and_remote_without_override() {
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Unknown, OriginOverride::None),
            OriginPolicy::Restrictive
        );
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Remote, OriginOverride::None),
            OriginPolicy::Restrictive
        );
        assert!(
            resolve_origin_policy(DetectedOrigin::Unknown, OriginOverride::None).is_restrictive()
        );
    }

    #[test]
    fn standard_only_for_local_without_override() {
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Local, OriginOverride::None),
            OriginPolicy::Standard
        );
        assert!(
            !resolve_origin_policy(DetectedOrigin::Local, OriginOverride::None).is_restrictive()
        );
    }

    #[test]
    fn forced_misclassification_falls_back_to_restrictive() {
        // End to end: whatever the adversary forges in the advisory signals,
        // the selected policy is restrictive unless a local-only signal (or
        // an explicit override) is present. Every non-local-only signal
        // combination resolves restrictive here.
        for signals in [
            OriginSignals::none(),
            OriginSignals::remote_only(),
            OriginSignals::conflicting(),
        ] {
            let detected = classify_origin(signals);
            assert_eq!(
                resolve_origin_policy(detected, OriginOverride::None),
                OriginPolicy::Restrictive,
                "signals {signals:?} (detected {detected}) must stay restrictive"
            );
        }
    }

    #[test]
    fn explicit_override_relaxes_unknown_and_remote() {
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Unknown, OriginOverride::RelaxToStandard),
            OriginPolicy::Standard
        );
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Remote, OriginOverride::RelaxToStandard),
            OriginPolicy::Standard
        );
        assert_eq!(
            resolve_origin_policy(DetectedOrigin::Local, OriginOverride::RelaxToStandard),
            OriginPolicy::Standard
        );
    }

    #[test]
    fn no_implicit_relaxation_path() {
        // The default override is `None`, and `None` never relaxes a
        // non-local detection: relaxation requires spelling out
        // `RelaxToStandard`.
        assert_eq!(OriginOverride::default(), OriginOverride::None);
        for detected in [
            DetectedOrigin::Local,
            DetectedOrigin::Remote,
            DetectedOrigin::Unknown,
        ] {
            let without_override = resolve_origin_policy(detected, OriginOverride::default());
            let explicit_none = resolve_origin_policy(detected, OriginOverride::None);
            assert_eq!(without_override, explicit_none);
        }
        // Detection alone cannot express an override: every classified
        // non-local origin stays restrictive.
        for detected in [DetectedOrigin::Remote, DetectedOrigin::Unknown] {
            assert!(
                resolve_origin_policy(detected, OriginOverride::None).is_restrictive(),
                "detected {detected} must stay restrictive without an explicit override"
            );
        }
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(DetectedOrigin::Local.to_string(), "local");
        assert_eq!(DetectedOrigin::Remote.to_string(), "remote");
        assert_eq!(DetectedOrigin::Unknown.to_string(), "unknown");
        assert_eq!(OriginPolicy::Standard.to_string(), "standard");
        assert_eq!(OriginPolicy::Restrictive.to_string(), "restrictive");
    }
}
