#![forbid(unsafe_code)]
//! Generic panel-session scaffolding shared by the staged panel modules.
//!
//! This is the private machinery crate-internal seam introduced by the
//! CTX-0438 extraction wave: panel creation and registry-config validation
//! go through the accepted public Panel Runtime path exactly once, so every
//! staged experience module stays a thin adapter over
//! `create_panel` → `mount_panel` and `PanelRegistryConfig::validate`.
//!
//! The helpers add no authority, no capability, and no wire shape: they are
//! straight delegations to the public `bitty_runtime::registry` API with the
//! same typed errors. They stay private until the panel-provider contract
//! (OQ-058) defines a public provider surface.

use bitty_runtime::registry::{
    PanelError, PanelId, PanelRegistry, PanelRegistryConfig, PanelType, WorkspaceId,
};
use bitty_ui::ViewId;

/// Creates a panel of `ty` in `workspace` and mounts it into `view` through
/// the public Panel Runtime path (`create_panel` → `mount_panel`), returning
/// the created `PanelId`. Errors are the typed `PanelError` values from the
/// registry; no state is mutated on failure beyond the registry's own
/// documented behavior.
pub(crate) fn create_mounted_panel(
    registry: &mut PanelRegistry,
    ty: PanelType,
    workspace: WorkspaceId,
    view: ViewId,
) -> Result<PanelId, PanelError> {
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates a `PanelRegistryConfig` through the public typed path
/// (`PanelRegistryConfig::validate`), preserving the fail-closed bounds
/// (`[1,32]` per workspace, `[1,64]` per window, `[1,256]` topics,
/// `[1,32]` subscriptions per panel).
pub(crate) fn validate_registry_config(cfg: &PanelRegistryConfig) -> Result<(), PanelError> {
    cfg.validate()
}
