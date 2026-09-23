//! Trust-level model with per-level capability domains (OQ-085).
//!
//! `OQ-085` is adopted (register accepted 2026-09-23): level 0 Core,
//! 1 bundled Lua, 2 third-party Lua, 3 native sidecar, 4 external
//! tools/MCP/network, as pure, bounded, fail-closed data with the
//! per-level admission matrix below.
//!
//! One seam is enforced at a live call boundary:
//! [`TrustLevel::check_family`] denies families outside the level's admitted
//! domains, and [`crate::effective::authorize_with_trust`] runs that gate
//! before the six-layer grant intersection. No caller grants authority from
//! a [`TrustLevel`] alone, and unknown levels or domains deny rather than
//! default. The accepted capability grammar ([`crate::capability`]) and
//! the deny-by-default grant lifecycle ([`crate::grant`]) stay
//! authoritative; this kernel answers "would level L admit domain D" and
//! the effective boundary enforces the answer.
//!
//! # Non-goals
//!
//! Numeric level semantics beyond ordering and per-domain parameter bounds
//! stay open. There is no `unsafe`, no I/O, and no new dependency
//! (`std` only).

#![forbid(unsafe_code)]

use std::fmt;

use crate::capability::CapabilityFamily;
use crate::error::PluginError;

// ── bounds ────────────────────────────────────────────────────────────────

/// Maximum bytes of a trust-level label accepted by [`TrustLevel::parse`].
pub const MAX_TRUST_LABEL_BYTES: usize = 32;

// ── trust levels ──────────────────────────────────────────────────────────

/// Candidate trust level for a plugin, helper, or tool boundary (OQ-085).
///
/// Lower is more trusted. Ordering is structural only: it lets a caller
/// compare levels, never to grant authority by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrustLevel {
    /// Level 0: Bitty Core itself. Not granted — it *is* the host.
    Core,
    /// Level 1: bundled first-party Lua (shipped with the distribution).
    BundledLua,
    /// Level 2: third-party Lua plugins (registry or local path).
    ThirdPartyLua,
    /// Level 3: native sidecar processes (compiled helpers).
    NativeSidecar,
    /// Level 4: external tools, MCP servers, and network services.
    ExternalTool,
}

impl TrustLevel {
    /// Numeric level (`0`–`4`); lower is more trusted.
    #[must_use]
    pub const fn level_number(self) -> u8 {
        match self {
            Self::Core => 0,
            Self::BundledLua => 1,
            Self::ThirdPartyLua => 2,
            Self::NativeSidecar => 3,
            Self::ExternalTool => 4,
        }
    }

    /// Stable label for diagnostics and doctor output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::BundledLua => "bundled-lua",
            Self::ThirdPartyLua => "third-party-lua",
            Self::NativeSidecar => "native-sidecar",
            Self::ExternalTool => "external-tool",
        }
    }

    /// Parse a level label; unknown labels fail closed.
    pub fn parse(s: &str) -> Result<Self, PluginError> {
        if s.len() > MAX_TRUST_LABEL_BYTES {
            return Err(PluginError::LimitExceeded {
                field: "trust_level".to_string(),
                limit: MAX_TRUST_LABEL_BYTES,
                actual: s.len(),
            });
        }
        match s {
            "core" => Ok(Self::Core),
            "bundled-lua" => Ok(Self::BundledLua),
            "third-party-lua" => Ok(Self::ThirdPartyLua),
            "native-sidecar" => Ok(Self::NativeSidecar),
            "external-tool" => Ok(Self::ExternalTool),
            _ => Err(PluginError::capability(
                s.to_string(),
                "unknown trust level (OQ-085 candidate; deny by default)",
            )),
        }
    }

    /// Candidate capability domains admitted at this level (OQ-085).
    ///
    /// Higher (less trusted) levels admit fewer domains; level 4 admits
    /// none by default — an external tool acts only through an explicit
    /// per-invocation grant, which this kernel does not model.
    #[must_use]
    pub const fn allowed_domains(self) -> &'static [CapabilityDomain] {
        match self {
            Self::Core => &[
                CapabilityDomain::Filesystem,
                CapabilityDomain::Network,
                CapabilityDomain::Process,
                CapabilityDomain::Clipboard,
                CapabilityDomain::Environment,
                CapabilityDomain::Credentials,
                CapabilityDomain::TerminalInput,
                CapabilityDomain::TerminalOutput,
                CapabilityDomain::Ipc,
                CapabilityDomain::Gpu,
            ],
            Self::BundledLua => &[
                CapabilityDomain::Filesystem,
                CapabilityDomain::Network,
                CapabilityDomain::Process,
                CapabilityDomain::Clipboard,
                CapabilityDomain::Environment,
                CapabilityDomain::TerminalInput,
                CapabilityDomain::TerminalOutput,
                CapabilityDomain::Ipc,
            ],
            Self::ThirdPartyLua => &[
                CapabilityDomain::Filesystem,
                CapabilityDomain::Clipboard,
                CapabilityDomain::Environment,
                CapabilityDomain::TerminalOutput,
                CapabilityDomain::Ipc,
            ],
            Self::NativeSidecar => &[
                CapabilityDomain::Filesystem,
                CapabilityDomain::TerminalOutput,
                CapabilityDomain::Ipc,
            ],
            Self::ExternalTool => &[],
        }
    }

    /// Whether this level admits `domain` (pure candidate check).
    #[must_use]
    pub const fn admits(self, domain: CapabilityDomain) -> bool {
        let allowed = self.allowed_domains();
        let mut i = 0;
        while i < allowed.len() {
            if allowed[i] as u8 == domain as u8 {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Check one accepted capability family against this level (OQ-085 adopted).
    ///
    /// Call-boundary gate enforced by [`crate::effective::authorize_with_trust`]:
    /// families with a domain mapping must be admitted by this level, otherwise
    /// the request denies fail-closed with a grant error naming the level and
    /// the family only (never a value). Families without a domain mapping
    /// (`Ui`, `Runtime`, `Agent`, `Mcp`, `Ai`, and the rest) pass here: the
    /// adopted matrix says nothing about them, so their grants decide
    /// elsewhere. The check is coarse at the shared `Terminal` family: it
    /// passes when the level admits either `TerminalInput` or
    /// `TerminalOutput`; per-head narrowing stays with the grant intersection.
    pub fn check_family(self, family: CapabilityFamily) -> Result<(), PluginError> {
        let domains = CapabilityDomain::for_family(family);
        if domains.is_empty() {
            return Ok(());
        }
        for domain in domains {
            if self.admits(*domain) {
                return Ok(());
            }
        }
        Err(PluginError::grant(format!(
            "trust level '{}' does not admit '{}' capabilities (OQ-085 adopted)",
            self.as_str(),
            family.as_str()
        )))
    }
}

impl fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── capability domains ────────────────────────────────────────────────────

/// Candidate capability domains from OQ-085 (one per trust-boundary kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityDomain {
    Filesystem,
    Network,
    Process,
    Clipboard,
    Environment,
    Credentials,
    TerminalInput,
    TerminalOutput,
    Ipc,
    Gpu,
}

impl CapabilityDomain {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filesystem => "filesystem",
            Self::Network => "network",
            Self::Process => "process",
            Self::Clipboard => "clipboard",
            Self::Environment => "environment",
            Self::Credentials => "credentials",
            Self::TerminalInput => "terminal-input",
            Self::TerminalOutput => "terminal-output",
            Self::Ipc => "ipc",
            Self::Gpu => "gpu",
        }
    }

    /// Accepted [`CapabilityFamily`] identifiers that live under this
    /// candidate domain. Domains without an accepted family map to an
    /// empty set: the candidate never invents authority the accepted
    /// grammar does not already name.
    #[must_use]
    pub const fn accepted_families(self) -> &'static [CapabilityFamily] {
        match self {
            Self::Filesystem => &[CapabilityFamily::Fs],
            Self::Network => &[CapabilityFamily::Network],
            Self::Process => &[CapabilityFamily::Process],
            Self::Clipboard => &[CapabilityFamily::Clipboard],
            // Environment reads are host-mediated (`bitty.env.get`
            // allowlist); no accepted family names them, so the
            // candidate claims none.
            Self::Environment => &[],
            // Credentials resolve as opaque `secret://` handles; no
            // accepted family names them, so the candidate claims none.
            Self::Credentials => &[],
            Self::TerminalInput => &[CapabilityFamily::Terminal],
            Self::TerminalOutput => &[CapabilityFamily::Terminal],
            // IPC transport is host-owned; no accepted family names it.
            Self::Ipc => &[],
            // No GPU capability family exists in the accepted grammar.
            Self::Gpu => &[],
        }
    }

    /// Adopted domains covering one accepted family (reverse of
    /// [`CapabilityDomain::accepted_families`]).
    ///
    /// Families the adopted matrix does not cover map to an empty set: the
    /// trust gate passes them through and their grants decide elsewhere, so
    /// the matrix never invents authority the accepted grammar does not
    /// already name.
    #[must_use]
    pub const fn for_family(family: CapabilityFamily) -> &'static [CapabilityDomain] {
        match family {
            CapabilityFamily::Fs => &[CapabilityDomain::Filesystem],
            CapabilityFamily::Network => &[CapabilityDomain::Network],
            CapabilityFamily::Process => &[CapabilityDomain::Process],
            CapabilityFamily::Clipboard => &[CapabilityDomain::Clipboard],
            CapabilityFamily::Terminal => &[
                CapabilityDomain::TerminalInput,
                CapabilityDomain::TerminalOutput,
            ],
            CapabilityFamily::Ui
            | CapabilityFamily::Runtime
            | CapabilityFamily::Env
            | CapabilityFamily::Debug
            | CapabilityFamily::Platform
            | CapabilityFamily::Protocol
            | CapabilityFamily::Panel
            | CapabilityFamily::Browser
            | CapabilityFamily::Layout
            | CapabilityFamily::Agent
            | CapabilityFamily::Mcp
            | CapabilityFamily::Ai => &[],
        }
    }
}

impl fmt::Display for CapabilityDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip() {
        for level in [
            TrustLevel::Core,
            TrustLevel::BundledLua,
            TrustLevel::ThirdPartyLua,
            TrustLevel::NativeSidecar,
            TrustLevel::ExternalTool,
        ] {
            assert_eq!(TrustLevel::parse(level.as_str()), Ok(level));
            assert_eq!(level.to_string(), level.as_str());
        }
    }

    #[test]
    fn level_numbers_order() {
        assert!(TrustLevel::Core.level_number() < TrustLevel::BundledLua.level_number());
        assert!(TrustLevel::BundledLua.level_number() < TrustLevel::ThirdPartyLua.level_number());
        assert!(
            TrustLevel::ThirdPartyLua.level_number() < TrustLevel::NativeSidecar.level_number()
        );
        assert!(TrustLevel::NativeSidecar.level_number() < TrustLevel::ExternalTool.level_number());
        assert!(TrustLevel::Core < TrustLevel::ExternalTool);
    }

    #[test]
    fn unknown_level_denies() {
        assert!(TrustLevel::parse("kernel").is_err());
        assert!(TrustLevel::parse("").is_err());
        assert!(TrustLevel::parse("CORE").is_err());
    }

    #[test]
    fn oversize_label_denies() {
        let long = "x".repeat(MAX_TRUST_LABEL_BYTES + 1);
        assert!(TrustLevel::parse(&long).is_err());
    }

    #[test]
    fn domains_narrow_with_level() {
        let core = TrustLevel::Core.allowed_domains().len();
        let bundled = TrustLevel::BundledLua.allowed_domains().len();
        let third = TrustLevel::ThirdPartyLua.allowed_domains().len();
        let sidecar = TrustLevel::NativeSidecar.allowed_domains().len();
        let external = TrustLevel::ExternalTool.allowed_domains().len();
        assert!(core > bundled);
        assert!(bundled > third);
        assert!(third > sidecar);
        assert_eq!(external, 0);
    }

    #[test]
    fn credentials_admitted_only_at_core() {
        assert!(TrustLevel::Core.admits(CapabilityDomain::Credentials));
        assert!(!TrustLevel::BundledLua.admits(CapabilityDomain::Credentials));
        assert!(!TrustLevel::ThirdPartyLua.admits(CapabilityDomain::Credentials));
        assert!(!TrustLevel::ExternalTool.admits(CapabilityDomain::TerminalOutput));
    }

    #[test]
    fn domain_labels_stable() {
        assert_eq!(CapabilityDomain::Filesystem.to_string(), "filesystem");
        assert_eq!(
            CapabilityDomain::TerminalInput.to_string(),
            "terminal-input"
        );
        assert_eq!(CapabilityDomain::Gpu.to_string(), "gpu");
    }

    #[test]
    fn accepted_family_mapping_claims_nothing_unaccepted() {
        assert_eq!(
            CapabilityDomain::Filesystem.accepted_families(),
            &[CapabilityFamily::Fs]
        );
        assert_eq!(
            CapabilityDomain::Network.accepted_families(),
            &[CapabilityFamily::Network]
        );
        assert!(CapabilityDomain::Environment.accepted_families().is_empty());
        assert!(CapabilityDomain::Credentials.accepted_families().is_empty());
        assert!(CapabilityDomain::Ipc.accepted_families().is_empty());
        assert!(CapabilityDomain::Gpu.accepted_families().is_empty());
    }

    #[test]
    fn for_family_covers_exactly_the_mapped_domains() {
        assert_eq!(
            CapabilityDomain::for_family(CapabilityFamily::Fs),
            &[CapabilityDomain::Filesystem]
        );
        assert_eq!(
            CapabilityDomain::for_family(CapabilityFamily::Terminal),
            &[
                CapabilityDomain::TerminalInput,
                CapabilityDomain::TerminalOutput
            ]
        );
        assert!(CapabilityDomain::for_family(CapabilityFamily::Agent).is_empty());
        assert!(CapabilityDomain::for_family(CapabilityFamily::Ai).is_empty());
        assert!(CapabilityDomain::for_family(CapabilityFamily::Ui).is_empty());
        // `Env` reads are host-mediated per-key grants (`bitty.env`); the
        // trust matrix claims no domain for them (added with #1308).
        assert!(CapabilityDomain::for_family(CapabilityFamily::Env).is_empty());
    }

    #[test]
    fn family_gate_denies_outside_admission() {
        // External tools admit no domain: every mapped family denies.
        for family in [
            CapabilityFamily::Fs,
            CapabilityFamily::Network,
            CapabilityFamily::Process,
            CapabilityFamily::Clipboard,
            CapabilityFamily::Terminal,
        ] {
            assert!(
                TrustLevel::ExternalTool.check_family(family).is_err(),
                "{} must deny {}",
                TrustLevel::ExternalTool,
                family.as_str()
            );
        }
        // Core admits everything mapped.
        for family in [
            CapabilityFamily::Fs,
            CapabilityFamily::Network,
            CapabilityFamily::Process,
            CapabilityFamily::Clipboard,
            CapabilityFamily::Terminal,
        ] {
            assert!(
                TrustLevel::Core.check_family(family).is_ok(),
                "{} must admit {}",
                TrustLevel::Core,
                family.as_str()
            );
        }
    }

    #[test]
    fn family_gate_narrows_with_level() {
        // Bundled Lua keeps network and process; the native sidecar loses both.
        assert!(
            TrustLevel::BundledLua
                .check_family(CapabilityFamily::Network)
                .is_ok()
        );
        assert!(
            TrustLevel::BundledLua
                .check_family(CapabilityFamily::Process)
                .is_ok()
        );
        assert!(
            TrustLevel::NativeSidecar
                .check_family(CapabilityFamily::Network)
                .is_err()
        );
        assert!(
            TrustLevel::NativeSidecar
                .check_family(CapabilityFamily::Fs)
                .is_ok()
        );
        // Third-party Lua keeps the shared terminal family through its
        // admitted output domain; per-head narrowing stays with grants.
        assert!(
            TrustLevel::ThirdPartyLua
                .check_family(CapabilityFamily::Terminal)
                .is_ok()
        );
    }

    #[test]
    fn unmapped_families_defer_to_grants() {
        // The adopted matrix covers no agent/AI/UI families: trust passes
        // them through at every level and their grants decide elsewhere.
        for family in [
            CapabilityFamily::Agent,
            CapabilityFamily::Mcp,
            CapabilityFamily::Ai,
            CapabilityFamily::Ui,
        ] {
            for level in [
                TrustLevel::Core,
                TrustLevel::BundledLua,
                TrustLevel::ThirdPartyLua,
                TrustLevel::NativeSidecar,
                TrustLevel::ExternalTool,
            ] {
                assert!(
                    level.check_family(family).is_ok(),
                    "{level} must defer {} to grants",
                    family.as_str()
                );
            }
        }
    }

    #[test]
    fn family_denial_names_level_and_family_only() {
        let error = TrustLevel::ExternalTool
            .check_family(CapabilityFamily::Fs)
            .expect_err("external tools must deny filesystem");
        let text = error.to_string();
        assert!(text.contains("external-tool"), "{text}");
        assert!(text.contains("'fs'"), "{text}");
    }
}
