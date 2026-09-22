#![forbid(unsafe_code)]
//! Panel provider contract (CW-23, issue #1001; `OQ-058`).
//!
//! Contract source: the accepted Panel Runtime RFC defines a
//! `PanelProvider` as the plugin-supplied factory declaring one or more
//! `PanelType` values behind the `panel.provider` capability, while the
//! candidate Workspace Panel Invariants (`OQ-058`) keep the
//! workspace/session lifecycle coupling those providers depend on open.
//!
//! Status: the in-tree `bitty-panels` scaffolding stays crate-private
//! until this contract lands; this module is the public surface that
//! lifts it:
//!
//! - [`PANEL_PROVIDER_CAPABILITY`] — the closed host capability gating
//!   provider registration (`panel.provider`, already in the host set).
//! - [`PanelProviderManifest`] — validated declaration: `owner.name`
//!   provider id, non-empty deduplicated `PanelType` list, schema version.
//! - [`PanelProvider`] — the public provider trait. Declarative only:
//!   providers declare types, the Core-owned host creates and mounts
//!   panels. There is deliberately no `render`/`handle_event` hot trait
//!   (rejected: it would put Lua on the hot path and break invariant 4).
//! - [`PanelProviderRegistry`] — name-keyed provider set with the
//!   capability gate, duplicate rejection, and per-registration generation
//!   so reload/unload swaps provider content atomically.
//!
//! Capability *enforcement* for live panels stays with the host registry;
//! this registry stores the grant flag carried at registration so the
//! contract never depends on `bitty-plugin-host` direction.

use std::collections::HashMap;

use super::panel::PanelType;

// ---------------------------------------------------------------------------
// Bounds and capability
// ---------------------------------------------------------------------------

/// Capability gating provider registration. Mirrors the closed host
/// identifier `panel.provider`; the registry takes an explicit grant flag
/// instead of depending on the host crate.
pub const PANEL_PROVIDER_CAPABILITY: &str = "panel.provider";

/// Maximum panel types one provider may declare (closed v1 set has five).
pub const MAX_PROVIDER_TYPES: usize = 8;

/// Maximum `owner`/`name` segment length (mirrors the topic owner grammar).
pub const MAX_PROVIDER_OWNER_SEGMENT_LEN: usize = 16;

/// Manifest schema version accepted by v1.
pub const PROVIDER_MANIFEST_VERSION_V1: u32 = 1;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed provider-contract failure; registry state is unchanged on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderContractError {
    /// Registration without the `panel.provider` capability.
    CapabilityDenied { owner: String },
    /// Owner id already registered.
    DuplicateOwner { owner: String },
    /// Unknown owner id.
    UnknownOwner { owner: String },
    /// Malformed `owner.name` provider id.
    InvalidOwner { value: String, reason: String },
    /// No panel type declared.
    EmptyTypes,
    /// More types than [`MAX_PROVIDER_TYPES`].
    TooManyTypes { max: usize, current: usize },
    /// Same panel type declared twice.
    DuplicateType { panel_type: String },
    /// Unsupported manifest schema version.
    UnsupportedVersion { version: u32 },
}

impl std::fmt::Display for ProviderContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityDenied { owner } => {
                write!(f, "provider '{owner}' missing capability 'panel.provider'")
            }
            Self::DuplicateOwner { owner } => {
                write!(f, "provider '{owner}' is already registered")
            }
            Self::UnknownOwner { owner } => write!(f, "unknown provider '{owner}'"),
            Self::InvalidOwner { value, reason } => {
                write!(f, "invalid provider id '{value}': {reason}")
            }
            Self::EmptyTypes => f.write_str("provider must declare at least one panel type"),
            Self::TooManyTypes { max, current } => {
                write!(f, "too many panel types: max {max}, current {current}")
            }
            Self::DuplicateType { panel_type } => {
                write!(f, "duplicate panel type '{panel_type}'")
            }
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported provider manifest version {version}")
            }
        }
    }
}

impl std::error::Error for ProviderContractError {}

// ---------------------------------------------------------------------------
// Manifest and provider trait
// ---------------------------------------------------------------------------

/// Validated provider declaration: who provides which panel types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelProviderManifest {
    owner: String,
    types: Vec<PanelType>,
    version: u32,
}

impl PanelProviderManifest {
    /// Validates `owner` (`owner.name` grammar), the type list (non-empty,
    /// bounded, deduplicated), and the schema version.
    ///
    /// # Errors
    ///
    /// [`ProviderContractError`] variants describing the first violation;
    /// nothing is allocated on behalf of a registry.
    pub fn parse(
        owner: &str,
        types: Vec<PanelType>,
        version: u32,
    ) -> Result<Self, ProviderContractError> {
        validate_owner(owner)?;
        if version != PROVIDER_MANIFEST_VERSION_V1 {
            return Err(ProviderContractError::UnsupportedVersion { version });
        }
        if types.is_empty() {
            return Err(ProviderContractError::EmptyTypes);
        }
        if types.len() > MAX_PROVIDER_TYPES {
            return Err(ProviderContractError::TooManyTypes {
                max: MAX_PROVIDER_TYPES,
                current: types.len(),
            });
        }
        let mut seen = Vec::with_capacity(types.len());
        for panel_type in &types {
            if seen.contains(panel_type) {
                return Err(ProviderContractError::DuplicateType {
                    panel_type: panel_type.as_str().to_string(),
                });
            }
            seen.push(*panel_type);
        }
        Ok(Self {
            owner: owner.to_string(),
            types,
            version,
        })
    }

    /// Provider id (`owner.name`).
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Declared panel types, in declaration order.
    #[must_use]
    pub fn types(&self) -> &[PanelType] {
        &self.types
    }

    /// Manifest schema version.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether this provider declares `panel_type`.
    #[must_use]
    pub fn declares(&self, panel_type: PanelType) -> bool {
        self.types.contains(&panel_type)
    }
}

fn validate_owner(owner: &str) -> Result<(), ProviderContractError> {
    let reject = |reason: &str| ProviderContractError::InvalidOwner {
        value: owner.to_string(),
        reason: reason.to_string(),
    };
    if owner.is_empty() {
        return Err(reject("provider id must not be empty"));
    }
    if owner.len() > 2 * MAX_PROVIDER_OWNER_SEGMENT_LEN + 1 {
        return Err(reject("provider id exceeds bounded length"));
    }
    if owner.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(reject("provider id must not contain whitespace"));
    }
    let segments: Vec<&str> = owner.split('.').collect();
    if segments.len() != 2 {
        return Err(reject("provider id must be owner.name"));
    }
    for segment in &segments {
        if segment.is_empty() || segment.len() > MAX_PROVIDER_OWNER_SEGMENT_LEN {
            return Err(reject("owner segments must be 1..=16 bytes"));
        }
        if !segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        {
            return Err(reject("owner segments must start with [a-z]"));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(reject("owner segments must use [a-z0-9_-]"));
        }
    }
    Ok(())
}

/// Public panel provider surface: a plugin-supplied factory declaring one
/// or more [`PanelType`] values.
///
/// Declarative only and object-safe (`&self` methods, no generics), so the
/// host can hold `dyn PanelProvider` without a hot trait. Providers never
/// mutate workspace, layout, or bar state directly; they declare types and
/// the Core-owned host drives lifecycle, bus mediation, and composition.
pub trait PanelProvider {
    /// Validated provider declaration.
    fn manifest(&self) -> &PanelProviderManifest;
}

// ---------------------------------------------------------------------------
// Provider registry
// ---------------------------------------------------------------------------

/// Name-keyed provider set with the `panel.provider` capability gate.
/// Duplicate owners are rejected, not shadowed; each registration mints a
/// fresh generation so reload swaps provider content atomically.
#[derive(Debug, Default)]
pub struct PanelProviderRegistry {
    providers: HashMap<String, (PanelProviderManifest, u64)>,
    next_generation: u64,
}

impl PanelProviderRegistry {
    /// Creates an empty provider registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a validated manifest. `capability_granted` carries the
    /// host's `panel.provider` grant; without it registration fails closed.
    /// Returns the registration generation.
    ///
    /// # Errors
    ///
    /// [`ProviderContractError::CapabilityDenied`] or
    /// [`ProviderContractError::DuplicateOwner`]; state is unchanged.
    pub fn register(
        &mut self,
        manifest: PanelProviderManifest,
        capability_granted: bool,
    ) -> Result<u64, ProviderContractError> {
        if !capability_granted {
            return Err(ProviderContractError::CapabilityDenied {
                owner: manifest.owner().to_string(),
            });
        }
        if self.providers.contains_key(manifest.owner()) {
            return Err(ProviderContractError::DuplicateOwner {
                owner: manifest.owner().to_string(),
            });
        }
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.providers
            .insert(manifest.owner().to_string(), (manifest, generation));
        Ok(generation)
    }

    /// Removes a provider; unload drops its contributed content without
    /// tearing down the host. Returns the removed manifest.
    ///
    /// # Errors
    ///
    /// [`ProviderContractError::UnknownOwner`]; state is unchanged.
    pub fn unregister(
        &mut self,
        owner: &str,
    ) -> Result<PanelProviderManifest, ProviderContractError> {
        self.providers.remove(owner).map_or_else(
            || {
                Err(ProviderContractError::UnknownOwner {
                    owner: owner.to_string(),
                })
            },
            |(manifest, _)| Ok(manifest),
        )
    }

    /// Returns the manifest for `owner`, if registered.
    #[must_use]
    pub fn get(&self, owner: &str) -> Option<&PanelProviderManifest> {
        self.providers.get(owner).map(|(manifest, _)| manifest)
    }

    /// Returns the registration generation for `owner`, if registered.
    #[must_use]
    pub fn generation_of(&self, owner: &str) -> Option<u64> {
        self.providers.get(owner).map(|(_, generation)| *generation)
    }

    /// Owners declaring `panel_type`, in lexicographic order.
    #[must_use]
    pub fn providers_for_type(&self, panel_type: PanelType) -> Vec<String> {
        let mut owners: Vec<String> = self
            .providers
            .iter()
            .filter(|(_, (manifest, _))| manifest.declares(panel_type))
            .map(|(owner, _)| owner.clone())
            .collect();
        owners.sort();
        owners
    }

    /// Number of registered providers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether no provider is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(owner: &str, types: Vec<PanelType>) -> PanelProviderManifest {
        PanelProviderManifest::parse(owner, types, PROVIDER_MANIFEST_VERSION_V1).unwrap()
    }

    #[test]
    fn valid_manifest_parses() {
        let parsed = manifest("example.git", vec![PanelType::Terminal, PanelType::Rich]);
        assert_eq!(parsed.owner(), "example.git");
        assert!(parsed.declares(PanelType::Terminal));
        assert!(!parsed.declares(PanelType::Browser));
        assert_eq!(parsed.version(), PROVIDER_MANIFEST_VERSION_V1);
    }

    #[test]
    fn invalid_owners_rejected() {
        for owner in [
            "",
            "example",
            "Example.git",
            "example.git.tool",
            "example.git ",
        ] {
            assert!(
                PanelProviderManifest::parse(owner, vec![PanelType::Terminal], 1).is_err(),
                "{owner}"
            );
        }
    }

    #[test]
    fn empty_duplicate_and_version_violations_rejected() {
        assert_eq!(
            PanelProviderManifest::parse("example.git", vec![], 1),
            Err(ProviderContractError::EmptyTypes)
        );
        assert_eq!(
            PanelProviderManifest::parse(
                "example.git",
                vec![PanelType::Terminal, PanelType::Terminal],
                1
            ),
            Err(ProviderContractError::DuplicateType {
                panel_type: "terminal".to_string(),
            })
        );
        assert_eq!(
            PanelProviderManifest::parse("example.git", vec![PanelType::Terminal], 99),
            Err(ProviderContractError::UnsupportedVersion { version: 99 })
        );
    }

    #[test]
    fn registration_requires_capability() {
        let mut registry = PanelProviderRegistry::new();
        let err = registry
            .register(manifest("example.git", vec![PanelType::Terminal]), false)
            .unwrap_err();
        assert_eq!(
            err,
            ProviderContractError::CapabilityDenied {
                owner: "example.git".to_string(),
            }
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn register_unregister_roundtrip_with_generations() {
        let mut registry = PanelProviderRegistry::new();
        let first = registry
            .register(manifest("example.git", vec![PanelType::Terminal]), true)
            .unwrap();
        let second = registry
            .register(manifest("example.ai", vec![PanelType::Helper]), true)
            .unwrap();
        assert!(second > first);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.generation_of("example.git"), Some(first));
        assert_eq!(
            registry.providers_for_type(PanelType::Terminal),
            ["example.git".to_string()]
        );
        let removed = registry.unregister("example.git").unwrap();
        assert_eq!(removed.owner(), "example.git");
        assert!(registry.get("example.git").is_none());
        assert_eq!(
            registry.unregister("example.git"),
            Err(ProviderContractError::UnknownOwner {
                owner: "example.git".to_string(),
            })
        );
    }

    #[test]
    fn duplicate_owner_rejected_not_shadowed() {
        let mut registry = PanelProviderRegistry::new();
        registry
            .register(manifest("example.git", vec![PanelType::Terminal]), true)
            .unwrap();
        let err = registry
            .register(manifest("example.git", vec![PanelType::Rich]), true)
            .unwrap_err();
        assert_eq!(
            err,
            ProviderContractError::DuplicateOwner {
                owner: "example.git".to_string(),
            }
        );
        assert!(
            registry
                .get("example.git")
                .unwrap()
                .declares(PanelType::Terminal)
        );
    }
}
