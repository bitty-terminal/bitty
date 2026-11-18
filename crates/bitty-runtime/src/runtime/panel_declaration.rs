//! Shipped provider-contract declarations (bitty#3243, closing condition).
//!
//! bitty#3239 row 13's restore half has two convergence inputs: the durable
//! panel records and the provider descriptor a plugin registers for the kinds
//! it drives. The records got a production producer and consumer in
//! [`crate::runtime::panel_persist`]; the descriptor's only production source
//! until this module was an *observation*
//! ([`Runtime::install_shared_scene_host`](crate::Runtime::install_shared_scene_host)
//! → `Runtime::register_live_panel_providers`), which mints
//! [`PROVIDER_MANIFEST_VERSION_V1`](crate::registry::PROVIDER_MANIFEST_VERSION_V1)
//! because a process can only attest the contract it read a container under.
//! A plugin that speaks a newer contract therefore had no way to say so in a
//! shipped binary: the registry admitted such a declaration, the bind edge
//! refused it, and the restore presented `incompatible` — but only ever with a
//! declaration a test had registered by hand. The skew was expressible, not
//! reachable.
//!
//! This module closes that: [`register_declared_panel_provider`] registers the
//! declaration a plugin package's own manifest carries
//! ([`bitty_plugin_host::manifest::PanelProviderDeclaration`], parsed from
//! `[panel.provider]` by
//! [`parse_manifest`](crate::plugin_runtime::manifest_toml::parse_manifest)),
//! at the composition root's activation edge
//! (`bitty-terminal`'s `plugin_runtime::discover_with_roots`), through the same
//! [`PanelRuntime::register_panel_provider`] the install uses. There is no
//! second registry and no second gate: the capability check, the duplicate rule,
//! and the `requires_newer_runtime` refusal are the ones already in place.
//!
//! # Why the activation edge and not the install
//!
//! A declaration is a fact about a *generation*, not about a live container: a
//! plugin may declare the kinds it drives before it has created one, and the
//! shipped order activates plugins (`discover_with_roots`) before the
//! composition root installs the host into the runtime
//! (`TerminalApp::with_panel_scene_host` → `install_shared_scene_host`). So the
//! declaration is registered first and the observation second, and the
//! observation's `DuplicateOwner` refusal — which the install already tolerates
//! — is exactly what keeps the *declared* version instead of shadowing it with
//! `V1`. That ordering is what makes a restored container present `incompatible`
//! in a real binary rather than in a test's world only.
//!
//! # What a declaration may not do
//!
//! Nothing here grants anything. The registry takes the caller's
//! `capability_granted` flag, and the composition root derives it from the
//! generation's own activation grant snapshot, so a plugin that requested
//! `panel.provider` and was denied registers nothing and its containers restore
//! `unloaded`/`denied` exactly as before. A declaration is also not authority
//! for the manifest hash (see [`PanelProviderDeclaration`]):
//! the only two facts it carries are a contract version, which can make the
//! runtime *stricter*, and a kind list, whose creation edge is already gated by
//! `panel.create`.

use std::fmt;

use bitty_plugin_host::manifest::PanelProviderDeclaration;

use crate::registry::{PanelProviderManifest, PanelRuntime, PanelType, ProviderContractError};

/// Why a declared provider contract was not registered.
///
/// The declaration is untrusted input that already passed the manifest reader;
/// this type reports the two places it can still fail to become a descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelDeclarationError {
    /// The declaration names a kind this runtime cannot map onto
    /// [`PanelType`]. The manifest reader refuses such a name too; this is the
    /// in-process path (a manifest built by a bundled catalog, not parsed from
    /// bytes) failing closed the same way rather than registering a descriptor
    /// that declares a subset of what the plugin said it drives.
    UnknownPanelType {
        /// The name the declaration carried.
        panel_type: String,
    },
    /// The registry refused the declaration: no `panel.provider` grant, or a
    /// malformed owner id.
    Registry(ProviderContractError),
}

impl fmt::Display for PanelDeclarationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownPanelType { panel_type } => {
                write!(f, "declared panel type '{panel_type}' is not a panel kind")
            }
            Self::Registry(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PanelDeclarationError {}

impl From<ProviderContractError> for PanelDeclarationError {
    fn from(error: ProviderContractError) -> Self {
        Self::Registry(error)
    }
}

/// Registers one plugin generation's declared provider contract on `host`.
///
/// The production caller is the composition root's activation edge
/// (`bitty-terminal`'s `plugin_runtime::discover_with_roots`), once per
/// generation that came up `Active` and whose manifest carries a
/// `[panel.provider]` declaration. `capability_granted` is the generation's own
/// `panel.provider` decision, so a denied generation registers nothing.
///
/// # Registration semantics
///
/// - **The kind list maps to [`PanelType`] whole or not at all.** A name the
///   runtime cannot map refuses the declaration instead of registering a
///   descriptor that silently declares less than the plugin said it drives.
/// - **The declared contract is registered verbatim.** A version above
///   [`PROVIDER_MANIFEST_VERSION_V1`](crate::registry::PROVIDER_MANIFEST_VERSION_V1)
///   is admitted here (that is what makes the skew observable) and every bind
///   against it refuses, so a restored container presents `incompatible`.
/// - **An identical descriptor is not re-registered.** A generation that
///   re-declares what its owner already registered keeps the existing
///   registration and its generation counter, so the answer a restore reads does
///   not churn on a resume.
/// - **A changed declaration swaps the descriptor.** Reload replaces the
///   generation's contract, which the registry expresses as unregister plus
///   register (its `register` refuses a duplicate owner by design). The
///   replacement manifest is built before the old one is dropped, so a refused
///   replacement (no capability) leaves the previous descriptor in place.
///
/// # Errors
///
/// [`PanelDeclarationError::UnknownPanelType`] for a name outside the closed
/// kind vocabulary, and [`PanelDeclarationError::Registry`] for the registry's
/// own refusals (no `panel.provider` grant, malformed owner id).
pub fn register_declared_panel_provider(
    host: &mut PanelRuntime,
    owner: &str,
    declaration: &PanelProviderDeclaration,
    capability_granted: bool,
) -> Result<u64, PanelDeclarationError> {
    let mut types = Vec::with_capacity(declaration.types.len());
    for name in &declaration.types {
        let Some(panel_type) = PanelType::parse(name) else {
            return Err(PanelDeclarationError::UnknownPanelType {
                panel_type: name.clone(),
            });
        };
        if panel_type == PanelType::Terminal {
            return Err(PanelDeclarationError::UnknownPanelType {
                panel_type: name.clone(),
            });
        }
        types.push(panel_type);
    }
    // Built (and therefore validated) before anything is dropped: a refusal here
    // cannot leave the owner without the descriptor it had.
    let manifest = PanelProviderManifest::parse(owner, types, declaration.contract)?;
    if host.provider_manifest(owner) == Some(&manifest) {
        if let Some(generation) = host.provider_generation_of(owner) {
            return Ok(generation);
        }
    }
    // A changed declaration replaces the generation's contract; the registry
    // refuses duplicate owners, so the swap is unregister + register. The
    // capability is re-checked before anything is dropped: the registry would
    // refuse the replacement anyway, and a refused replacement must leave the
    // descriptor this owner already holds in place (bitty#3243 review F2).
    let previous = if host.provider_manifest(owner).is_some() {
        if !capability_granted {
            return Err(PanelDeclarationError::Registry(
                ProviderContractError::CapabilityDenied {
                    owner: owner.to_string(),
                },
            ));
        }
        host.unregister_panel_provider(owner).ok()
    } else {
        None
    };
    match host.register_panel_provider(manifest, capability_granted) {
        Ok(generation) => Ok(generation),
        Err(error) => {
            // Belt and braces: a refusal after the swap still restores what the
            // owner had. The manifest was registered under a grant, so the
            // restore carries the same capability answer the swap was given.
            if let Some(previous) = previous {
                let _ = host.register_panel_provider(previous, true);
            }
            Err(PanelDeclarationError::Registry(error))
        }
    }
}
