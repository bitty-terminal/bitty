#![forbid(unsafe_code)]
//! Panel-session scaffolding and staged-module smoke tests (CTX-0438).
//!
//! Verifies the extraction-wave seam introduced when `ai_panel` and
//! `mail_panel` moved out of `bitty-runtime` into `bitty-panels`: both staged
//! modules create and mount their panels through the public Panel Runtime
//! path, validate registry configs fail-closed, and assemble tiled layouts
//! from the generic `LayoutNode` primitives. The shared scaffolding helper is
//! private, so this suite exercises it only through the modules' public
//! functions.

use bitty_panels::{ai_panel, mail_panel};
use bitty_runtime::registry::{PanelError, PanelRegistry, PanelRegistryConfig, WorkspaceId};
use bitty_ui::{LayoutNode, View, ViewId};

#[test]
fn staged_panels_create_and_mount_through_public_path() {
    let mut reg = PanelRegistry::new(PanelRegistryConfig::default()).expect("panel registry");
    let ws = WorkspaceId::new(1);

    let ai = ai_panel::create_ai_panel(&mut reg, ws, ViewId::new(1)).expect("create ai panel");
    let mail =
        mail_panel::create_mail_panel(&mut reg, ws, ViewId::new(2)).expect("create mail panel");
    assert_ne!(ai, mail, "distinct panel identities");
    assert_eq!(reg.panel_count(), 2);

    // Single-owner mount: a second panel cannot claim a mounted view, and the
    // typed error comes from the public registry path.
    let err = mail_panel::create_mail_panel(&mut reg, ws, ViewId::new(1))
        .expect_err("view already hosts a panel");
    assert!(matches!(err, PanelError::AlreadyMounted { .. }));
}

#[test]
fn staged_config_validation_is_fail_closed_and_shared() {
    assert!(ai_panel::validate_ai_panel_config(&PanelRegistryConfig::default()).is_ok());
    assert!(mail_panel::validate_mail_panel_config(&PanelRegistryConfig::default()).is_ok());

    let zero_workspace = PanelRegistryConfig {
        max_panels_per_workspace: 0,
        ..PanelRegistryConfig::default()
    };
    assert!(ai_panel::validate_ai_panel_config(&zero_workspace).is_err());
    assert!(mail_panel::validate_mail_panel_config(&zero_workspace).is_err());

    let overflow_window = PanelRegistryConfig {
        max_panels_per_window: usize::MAX,
        ..PanelRegistryConfig::default()
    };
    assert!(ai_panel::validate_ai_panel_config(&overflow_window).is_err());
    assert!(mail_panel::validate_mail_panel_config(&overflow_window).is_err());

    let zero_topics = PanelRegistryConfig {
        max_topics_total: 0,
        ..PanelRegistryConfig::default()
    };
    assert!(ai_panel::validate_ai_panel_config(&zero_topics).is_err());
    assert!(mail_panel::validate_mail_panel_config(&zero_topics).is_err());

    let overflow_subscriptions = PanelRegistryConfig {
        max_subscriptions_per_panel: usize::MAX,
        ..PanelRegistryConfig::default()
    };
    assert!(ai_panel::validate_ai_panel_config(&overflow_subscriptions).is_err());
    assert!(mail_panel::validate_mail_panel_config(&overflow_subscriptions).is_err());
}

#[test]
fn staged_tiled_layouts_reuse_generic_primitives() {
    let main = View::new(ViewId::new(10), 80, 24);
    let secondary = View::new(ViewId::new(11), 40, 24);

    let ai_tiled = ai_panel::ai_panel_tiled_layout(main.clone(), Some(secondary.clone()), 0.5);
    assert!(matches!(ai_tiled, LayoutNode::Split { .. }));
    assert_eq!(ai_tiled.leaf_count(), 2);

    let mail_tiled = mail_panel::mail_panel_tiled_layout(main.clone(), Some(secondary), 0.5);
    assert!(matches!(mail_tiled, LayoutNode::Split { .. }));
    assert_eq!(mail_tiled.leaf_count(), 2);

    let solo = mail_panel::mail_panel_tiled_layout(main, None, 0.5);
    assert_eq!(solo.leaf_count(), 1);
}
