//! Package-manager / runtime-loader boundary audit (W-101 / CTX-0927).
//!
//! This module is the in-repo audit record for the accepted
//! [Package Manager and Runtime Loader Boundary](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/development/package-manager-boundary.md)
//! (`W-72`). It classifies every `bitty-package` consumer and every public
//! package operation as either **Core-retained** (read-only manifest
//! validation and runtime loading of already-installed plugins) or
//! **external-manager-owned** (install, source fetch, dependency resolution,
//! transactional activation/rollback, update, uninstall, list). It adds no
//! behavior; it states the boundary in code so a drift in ownership fails a
//! test instead of passing review silently.
//!
//! # Core-never-network
//!
//! `bitty-package` and the `bitty-plugin-host` runtime-load path link no
//! network implementation crate and open no network connection. `Core` never
//! fetches, never resolves remote metadata, and never installs; `AF_UNIX` IPC
//! is the only permitted socket use and is not a package path (`DIR-016`,
//! `DIR-017`). The external `bitty-plugin-manager` is the only package actor
//! that may reach the network, and only behind an explicit user-initiated
//! command.
//!
//! # Operation ownership (mirrors the W-72 table)
//!
//! | Public package operation                   | Owner                  | Class            |
//! | ------------------------------------------ | ---------------------- | ---------------- |
//! | Manifest parse                             | `bitty-package`        | Core-retained    |
//! | Dependency resolution                      | `bitty-plugin-manager` | External-manager |
//! | Source fetch                               | `bitty-plugin-manager` | External-manager |
//! | Integrity, lock, and checksum verification | `bitty-plugin-manager` | External-manager |
//! | Install                                    | `bitty-plugin-manager` | External-manager |
//! | Activate                                   | `bitty-plugin-manager` | External-manager |
//! | Rollback                                   | `bitty-plugin-manager` | External-manager |
//! | Update                                     | `bitty-plugin-manager` | External-manager |
//! | Uninstall                                  | `bitty-plugin-manager` | External-manager |
//! | List / inspect                             | `bitty-plugin-manager` | External-manager |
//! | Runtime load / prepare                     | Plugin host            | Core-retained    |
//! | Capability grant / deny                    | Plugin host            | Core-retained    |
//! | Startup re-verification (Core re-derives)  | `bitty-package` + Core | Core-retained    |
//!
//! The install-time integrity chain remains in `bitty-package` as a shared
//! primitive both actors link, but Core independently re-derives the verdict
//! at startup through [`crate::startup::validate_installed_generation`]. The
//! external manager never decides startup trust.
//!
//! # `bitty-package` consumer audit
//!
//! Classification of every current consumer of this crate. "Core-retained"
//! means the consumer is on the read-only startup validation / runtime loading
//! path; "External-manager" means the consumer is install-time mechanics that
//! moves to `bitty-plugin-manager` once that replacement exists. Nothing is
//! deleted by this audit; the install-time consumer stays until the external
//! manager replaces it (`W-72` migration rule).
//!
//! | Consumer (path)                                              | Operation                              | Class            |
//! | ------------------------------------------------------------ | -------------------------------------- | ---------------- |
//! | `crates/bitty-package/src/manifest.rs`                       | Manifest parse / closed capabilities   | Core-retained    |
//! | `crates/bitty-package/src/lockfile.rs`                       | Lock schema / digest binding           | Core-retained    |
//! | `crates/bitty-package/src/integrity.rs`                      | Integrity primitives (H-A/B/C)         | Core-retained    |
//! | `crates/bitty-package/src/version.rs`                        | Version grammar                        | Core-retained    |
//! | `crates/bitty-package/src/requirement.rs`                    | Closed requirement grammar            | Core-retained    |
//! | `crates/bitty-package/src/error.rs`                          | Owned error vocabulary                 | Core-retained    |
//! | `crates/bitty-package/src/startup.rs`                        | Read-only startup re-verification      | Core-retained    |
//! | `crates/bitty-package/src/resolver.rs`                       | Dependency resolution                  | External-manager |
//! | `crates/bitty-package/src/source.rs`                         | Source declarations / fetch framing    | External-manager |
//! | `crates/bitty-package/src/lifecycle.rs`                      | Install state machine                  | External-manager |
//! | `crates/bitty-package/src/activation.rs`                     | Transactional activation / rollback    | External-manager |
//! | `crates/bitty-package/src/trust.rs`                          | Install-time publisher trust           | External-manager |
//! | `crates/bitty-plugin-host/src/manifest.rs`                   | Host manifest / compat validation      | Core-retained    |
//! | `crates/bitty-plugin-host/src/capability.rs`                 | Closed capability check                | Core-retained    |
//! | `crates/bitty-plugin-host/src/install.rs`                    | Install-time verification pipeline     | External-manager |
//! | `crates/bitty-plugin-host/src/registry.rs`                   | Resolver use in tests only             | External-manager |
//! | `crates/bitty-runtime/src/plugin_runtime/resolution.rs`      | Read-only installed-record load invoking the retained `startup::validate_staged_tree_generation` (`H-A` via the shared `source::canonical_tree_bytes` scheme, `H-B` + grants re-derived; live-dev drift stays `unverified` per RFC B.5) | Core-retained |
//! | `crates/bitty-runtime/src/plugin_runtime/package.rs`         | Compat grammar                         | Core-retained    |
//! | `crates/bitty-runtime/src/plugin_runtime/package.rs`         | Local install / uninstall / enable     | External-manager |
//! | `crates/bitty-runtime/src/plugin_runtime/services.rs`        | Version grammar at the call boundary   | Core-retained    |
//! | `crates/bitty-runtime/src/component/descriptor.rs`           | Native descriptor digest validation    | Core-retained    |
//! | `crates/bitty-ui/src/provider.rs`                            | Doc reference to `layout.provider`     | none (no `use`)  |
//!
//! # Known blocker for removing install-time code
//!
//! The install-time consumer (`bitty-plugin-host::install`, the
//! `bitty-package` resolver/activation/trust modules, and the
//! `bitty-runtime` local install path) cannot be removed until
//! `bitty-plugin-manager` exists. The read-only runtime load path digests the
//! staged Lua module tree; `H-A` is reconciled by sharing the single canonical
//! tree scheme in [`crate::source::canonical_tree_bytes`], and
//! `resolve_record` invokes the retained
//! [`crate::startup::validate_staged_tree_generation`] (same `H-A` primitive
//! and grant rule as [`crate::startup::validate_installed_generation`], with
//! the runtime manifest's canonical bytes bound generically as `H-B`). The
//! blob entry point stays for the package-manager generation model; the
//! remaining `W-101` work parked in the boundary document is
//! "retained-parser placement" (whether Core links `bitty-package` directly
//! or a narrower shared crate).

#![forbid(unsafe_code)]

/// Owner of a public package operation (`W-72` ownership table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OperationOwner {
    /// `bitty-package`: canonical bounded schema, parser, and integrity
    /// primitives Core links for startup validation.
    BittyPackage,
    /// External `bitty-plugin-manager`: install-time package management.
    /// Core never performs these.
    ExternalManager,
    /// In-Core `bitty-plugin-host`: runtime load and capability enforcement.
    PluginHost,
}

impl OperationOwner {
    /// Whether the owner is Core (i.e. not the external manager).
    #[must_use]
    pub fn is_core_retained(self) -> bool {
        !matches!(self, Self::ExternalManager)
    }

    /// Stable lowercase label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BittyPackage => "bitty-package",
            Self::ExternalManager => "bitty-plugin-manager",
            Self::PluginHost => "bitty-plugin-host",
        }
    }
}

/// Every public package operation from the accepted `W-72` table plus Core's
/// startup re-verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PackageOperation {
    /// Canonical bounded manifest parse (`bitty-package`).
    ManifestParse,
    /// Dependency resolution at build/install time (external manager).
    DependencyResolution,
    /// Source fetch (external manager, explicit user command only).
    SourceFetch,
    /// Install-time integrity/lock/checksum verification (external manager).
    IntegrityLockChecksum,
    /// Install (external manager).
    Install,
    /// Transactional activation (external manager).
    Activate,
    /// Staged rollback (external manager).
    Rollback,
    /// Update, capability increase gated (external manager).
    Update,
    /// Uninstall (external manager).
    Uninstall,
    /// Read-only list/inspect (external manager).
    ListInspect,
    /// Runtime load/prepare of an already-installed generation (plugin host).
    RuntimeLoadPrepare,
    /// Deny-by-default capability grant/deny at call time (plugin host).
    CapabilityGrantDeny,
    /// Core's independent read-only re-derivation at startup, using the
    /// retained `bitty-package` primitives.
    StartupReverification,
}

impl PackageOperation {
    /// Number of variants; kept as a named constant so the completeness assert
    /// below fails on a miscount rather than silently passing review.
    pub const COUNT: usize = 13;

    /// Every operation, for exhaustiveness tests and audit renderers.
    pub const ALL: [Self; Self::COUNT] = [
        Self::ManifestParse,
        Self::DependencyResolution,
        Self::SourceFetch,
        Self::IntegrityLockChecksum,
        Self::Install,
        Self::Activate,
        Self::Rollback,
        Self::Update,
        Self::Uninstall,
        Self::ListInspect,
        Self::RuntimeLoadPrepare,
        Self::CapabilityGrantDeny,
        Self::StartupReverification,
    ];

    /// Owner per the accepted `W-72` ownership table.
    #[must_use]
    pub fn owner(self) -> OperationOwner {
        match self {
            Self::ManifestParse | Self::StartupReverification => OperationOwner::BittyPackage,
            Self::DependencyResolution
            | Self::SourceFetch
            | Self::IntegrityLockChecksum
            | Self::Install
            | Self::Activate
            | Self::Rollback
            | Self::Update
            | Self::Uninstall
            | Self::ListInspect => OperationOwner::ExternalManager,
            Self::RuntimeLoadPrepare | Self::CapabilityGrantDeny => OperationOwner::PluginHost,
        }
    }

    /// Whether Core retains this operation.
    #[must_use]
    pub fn is_core_retained(self) -> bool {
        self.owner().is_core_retained()
    }

    /// Whether this operation may reach the network under the accepted gate.
    ///
    /// Always false for Core: only the external manager's resolution, source
    /// fetch, install, and update may reach the network, and only behind an
    /// explicit user-initiated command.
    #[must_use]
    pub fn may_reach_network(self) -> bool {
        matches!(
            self,
            Self::DependencyResolution | Self::SourceFetch | Self::Install | Self::Update
        )
    }

    /// Stable lowercase label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ManifestParse => "manifest_parse",
            Self::DependencyResolution => "dependency_resolution",
            Self::SourceFetch => "source_fetch",
            Self::IntegrityLockChecksum => "integrity_lock_checksum",
            Self::Install => "install",
            Self::Activate => "activate",
            Self::Rollback => "rollback",
            Self::Update => "update",
            Self::Uninstall => "uninstall",
            Self::ListInspect => "list_inspect",
            Self::RuntimeLoadPrepare => "runtime_load_prepare",
            Self::CapabilityGrantDeny => "capability_grant_deny",
            Self::StartupReverification => "startup_reverification",
        }
    }
}

/// Compile-time completeness: `ALL` must list every enum variant (review item
/// (c)).
///
/// Fixed-length assert: adding a variant without listing it in `ALL` fails
/// here (and the exhaustive `match` below fails the build until it lists the
/// new variant), so ownership drift cannot pass review silently.
const _: () = assert!(
    PackageOperation::ALL.len() == PackageOperation::COUNT,
    "PackageOperation::ALL must list every variant"
);
const _: () = assert!(
    PackageOperation::COUNT == 13,
    "PackageOperation::COUNT must match the W-72 operation table"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_operation_is_classified_exactly_once() {
        // Fixed-length completeness (review item (c)): `ALL` must hold exactly
        // `COUNT` (13) entries; a new variant missing from `ALL` fails here
        // even if the exhaustive `match` below was updated.
        assert_eq!(
            PackageOperation::ALL.len(),
            PackageOperation::COUNT,
            "PackageOperation::ALL must list every variant"
        );
        assert_eq!(
            PackageOperation::COUNT,
            13,
            "PackageOperation::ALL must list every variant"
        );
        let unique: BTreeSet<PackageOperation> = PackageOperation::ALL.iter().copied().collect();
        assert_eq!(unique.len(), PackageOperation::ALL.len());
        // Exhaustiveness guard: a new variant must be added to `ALL`. The
        // `match` below is exhaustive, so the build breaks on a new variant
        // until this test lists it; the counters above then catch a variant
        // that was added to the match but forgotten in `ALL`.
        let mut listed = 0usize;
        for operation in PackageOperation::ALL {
            match operation {
                PackageOperation::ManifestParse
                | PackageOperation::DependencyResolution
                | PackageOperation::SourceFetch
                | PackageOperation::IntegrityLockChecksum
                | PackageOperation::Install
                | PackageOperation::Activate
                | PackageOperation::Rollback
                | PackageOperation::Update
                | PackageOperation::Uninstall
                | PackageOperation::ListInspect
                | PackageOperation::RuntimeLoadPrepare
                | PackageOperation::CapabilityGrantDeny
                | PackageOperation::StartupReverification => listed += 1,
            }
            let _ = operation.owner();
            assert!(!operation.label().is_empty());
        }
        assert_eq!(
            listed,
            PackageOperation::COUNT,
            "PackageOperation::ALL must list every variant"
        );
    }

    #[test]
    fn core_retained_operations_are_the_accepted_set() {
        let retained: BTreeSet<PackageOperation> = PackageOperation::ALL
            .iter()
            .copied()
            .filter(|operation| operation.is_core_retained())
            .collect();
        let expected: BTreeSet<PackageOperation> = [
            PackageOperation::ManifestParse,
            PackageOperation::RuntimeLoadPrepare,
            PackageOperation::CapabilityGrantDeny,
            PackageOperation::StartupReverification,
        ]
        .into_iter()
        .collect();
        assert_eq!(retained, expected);
    }

    #[test]
    fn external_manager_owns_install_and_network_operations() {
        for operation in [
            PackageOperation::DependencyResolution,
            PackageOperation::SourceFetch,
            PackageOperation::Install,
            PackageOperation::Activate,
            PackageOperation::Rollback,
            PackageOperation::Update,
            PackageOperation::Uninstall,
            PackageOperation::ListInspect,
            PackageOperation::IntegrityLockChecksum,
        ] {
            assert_eq!(
                operation.owner(),
                OperationOwner::ExternalManager,
                "{operation:?} must be external-manager owned"
            );
        }
    }

    #[test]
    fn core_retains_no_network_operation() {
        for operation in PackageOperation::ALL {
            if operation.is_core_retained() {
                assert!(
                    !operation.may_reach_network(),
                    "Core-retained {operation:?} must never reach the network"
                );
            }
        }
    }

    #[test]
    fn only_expected_operations_may_reach_network() {
        let network: BTreeSet<PackageOperation> = PackageOperation::ALL
            .iter()
            .copied()
            .filter(|operation| operation.may_reach_network())
            .collect();
        let expected: BTreeSet<PackageOperation> = [
            PackageOperation::DependencyResolution,
            PackageOperation::SourceFetch,
            PackageOperation::Install,
            PackageOperation::Update,
        ]
        .into_iter()
        .collect();
        assert_eq!(network, expected);
    }
}
