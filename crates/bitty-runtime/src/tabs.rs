#![forbid(unsafe_code)]
//! Deprecated `tabs` alias for the canonical [`crate::workspace`] module.
//!
//! ALIAS, not flag-day (removal ≥ v0.2.0 per DEC-0032 / CTX-0239 RFC).
//! Old id `bitty-terminal.tabs`, old commands `bitty-terminal.tabs:*`,
//! old claim `tabline`, old consts `TABS_*`, old type `TabsIntegration`,
//! and old fns `create_tabs_panel` / `validate_tabs_panel_config` remain
//! as `#[deprecated]` shims delegating to the `workspace` canonical names.
//! New code must use [`crate::workspace`]. This shim exists only so stored
//! grants, scripts, and third-party `tabline` claimants keep working during
//! the compat window. Test-only `xuepoo.tabs:*` strings get NO compat promise
//! and have been renamed outright.

use bitty_ui::{LayoutNode, View, ViewId};

use crate::workspace::{
    WORKSPACE_MAX_PANELS_PER_WINDOW, WORKSPACE_MAX_PANELS_PER_WORKSPACE, WORKSPACE_MAX_TABS,
    WORKSPACE_PAYLOAD_MAX_BYTES, WORKSPACE_TITLE_MAX_CHARS, WorkspaceIntegration as WorkspaceInner,
};

/// Deprecated alias: use [`crate::workspace::WORKSPACE_MAX_TABS`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WORKSPACE_MAX_TABS (tabs alias removal >= v0.2.0)"
)]
pub const TABS_MAX_TABS: usize = WORKSPACE_MAX_TABS;

/// Deprecated alias: use [`crate::workspace::WORKSPACE_MAX_PANELS_PER_WORKSPACE`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WORKSPACE_MAX_PANELS_PER_WORKSPACE (tabs alias removal >= v0.2.0)"
)]
pub const TABS_MAX_PANELS_PER_WORKSPACE: usize = WORKSPACE_MAX_PANELS_PER_WORKSPACE;

/// Deprecated alias: use [`crate::workspace::WORKSPACE_MAX_PANELS_PER_WINDOW`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WORKSPACE_MAX_PANELS_PER_WINDOW (tabs alias removal >= v0.2.0)"
)]
pub const TABS_MAX_PANELS_PER_WINDOW: usize = WORKSPACE_MAX_PANELS_PER_WINDOW;

/// Deprecated alias: use [`crate::workspace::WORKSPACE_PAYLOAD_MAX_BYTES`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WORKSPACE_PAYLOAD_MAX_BYTES (tabs alias removal >= v0.2.0)"
)]
pub const TABS_PAYLOAD_MAX_BYTES: usize = WORKSPACE_PAYLOAD_MAX_BYTES;

/// Deprecated alias: use [`crate::workspace::WORKSPACE_TITLE_MAX_CHARS`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WORKSPACE_TITLE_MAX_CHARS (tabs alias removal >= v0.2.0)"
)]
pub const TABS_TITLE_MAX_CHARS: usize = WORKSPACE_TITLE_MAX_CHARS;

/// Deprecated alias: use [`crate::workspace::WorkspaceIntegration`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::WorkspaceIntegration (tabs alias removal >= v0.2.0)"
)]
pub type TabsIntegration = WorkspaceInner;

#[allow(deprecated)]
impl TabsIntegration {
    /// Deprecated alias: use [`WorkspaceInner::stack_for_workspace`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::stack_for_workspace (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn stack_for_tabs(views: Vec<View>) -> LayoutNode {
        WorkspaceInner::stack_for_workspace(views)
    }

    /// Deprecated alias: use [`WorkspaceInner::split_for_workspace`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::split_for_workspace (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn split_for_tabs(left: Vec<View>, right: Vec<View>, ratio: f32) -> LayoutNode {
        WorkspaceInner::split_for_workspace(left, right, ratio)
    }

    /// Deprecated alias: use [`WorkspaceInner::workspace_count`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::workspace_count (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn tab_count(layout: &LayoutNode) -> usize {
        WorkspaceInner::workspace_count(layout)
    }

    /// Deprecated alias: use [`WorkspaceInner::workspace_ids`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::workspace_ids (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn tab_ids(layout: &LayoutNode) -> Vec<ViewId> {
        WorkspaceInner::workspace_ids(layout)
    }

    /// Deprecated alias: use [`WorkspaceInner::workspace_title`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::workspace_title (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn tab_title(state: &bitty_term_state::State) -> Option<String> {
        WorkspaceInner::workspace_title(state)
    }

    /// Deprecated alias: use [`WorkspaceInner::has_workspaces`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::has_workspaces (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn has_tabs(layout: &LayoutNode) -> bool {
        WorkspaceInner::has_workspaces(layout)
    }

    /// Deprecated alias: use [`WorkspaceInner::find_workspace`].
    #[deprecated(
        since = "0.1.0",
        note = "use WorkspaceIntegration::find_workspace (tabs alias removal >= v0.2.0)"
    )]
    #[must_use]
    pub fn find_tab(layout: &LayoutNode, id: ViewId) -> Option<View> {
        WorkspaceInner::find_workspace(layout, id)
    }
}

/// Deprecated alias: use [`crate::workspace::create_workspace_panel`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::create_workspace_panel (tabs alias removal >= v0.2.0)"
)]
pub fn create_tabs_panel(
    registry: &mut crate::registry::PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<crate::registry::PanelId, crate::registry::PanelError> {
    crate::workspace::create_workspace_panel(registry, workspace, view)
}

/// Deprecated alias: use [`crate::workspace::validate_workspace_panel_config`].
#[deprecated(
    since = "0.1.0",
    note = "use workspace::validate_workspace_panel_config (tabs alias removal >= v0.2.0)"
)]
pub fn validate_tabs_panel_config(
    cfg: &crate::registry::PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    crate::workspace::validate_workspace_panel_config(cfg)
}
