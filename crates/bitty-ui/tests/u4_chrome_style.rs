//! U-4 family integration coverage (UX-18/UX-19/UX-20, CTX-0669).
//!
//! Candidate behavior: window chrome surfaces, typed panel rules, and the
//! resolved style cascade working as one headless stack — a rule accent
//! joins the cascade at its origin rank, and chrome hosts the nodes the
//! tree defines.

#![forbid(unsafe_code)]

use bitty_ui::theme::TokenColor;
use bitty_ui::{
    ChromeError, ChromeNotification, ChromeSurface, EffectKind, NotificationId,
    NotificationSeverity, PanelId, PanelRule, PanelRuleSet, PanelType, PresentationMode,
    ResolvedStyle, RuleDiagnostic, RuleEffect, RuleError, RuleId, RuleOrigin, RuleSelector,
    SceneLayer, StyleCascade, StyleOrigin, UiNodeId, WindowChromeRuntime,
};

/// A user rule accent joins the cascade at the user-rule rank: it beats the
/// user theme but still loses to safety.
#[test]
fn rule_accent_joins_cascade_at_origin_rank() {
    let mut rules = PanelRuleSet::new();
    rules
        .add(PanelRule::new(
            RuleId::new(1),
            RuleOrigin::User,
            RuleSelector::Panel(PanelId::new(9)),
            RuleEffect::Accent(TokenColor::from_rgba([0x11, 0x22, 0x33, 0xFF])),
        ))
        .expect("fits");
    let winner = rules
        .resolve(PanelId::new(9), PanelType::Terminal, EffectKind::Accent)
        .expect("resolves")
        .expect("claimed");
    let RuleEffect::Accent(accent) = winner.effect() else {
        panic!("expected an accent effect");
    };

    let mut cascade = StyleCascade::new();
    cascade
        .set(
            StyleOrigin::UserTheme,
            "content.accent",
            TokenColor::from_rgba([0xAA, 0xBB, 0xCC, 0xFF]),
        )
        .expect("valid key");
    cascade
        .set(
            StyleOrigin::for_rule_origin(winner.origin()),
            "content.accent",
            accent,
        )
        .expect("rule accent joins the cascade");
    let resolved: ResolvedStyle = cascade.resolve("content.accent").expect("claimed");
    assert_eq!(resolved.value, accent);
    assert_eq!(resolved.origin, StyleOrigin::UserRule);
}

/// A workspace rule accent loses to a user rule accent end to end: panel
/// rules resolve the per-panel winner, the cascade resolves the per-key
/// winner, and both agree user beats workspace.
#[test]
fn user_rule_beats_workspace_rule_end_to_end() {
    let rules = PanelRuleSet::from_rules(vec![
        PanelRule::new(
            RuleId::new(1),
            RuleOrigin::Workspace,
            RuleSelector::AnyPanel,
            RuleEffect::Accent(TokenColor::from_rgba([0x01, 0x02, 0x03, 0xFF])),
        ),
        PanelRule::new(
            RuleId::new(2),
            RuleOrigin::User,
            RuleSelector::PanelType(PanelType::Terminal),
            RuleEffect::Accent(TokenColor::from_rgba([0x04, 0x05, 0x06, 0xFF])),
        ),
    ])
    .expect("fits");
    let winner = rules
        .resolve(PanelId::new(9), PanelType::Terminal, EffectKind::Accent)
        .expect("resolves")
        .expect("claimed");
    assert_eq!(winner.id(), RuleId::new(2));

    let mut cascade = StyleCascade::new();
    for rule in rules.rules() {
        if let RuleEffect::Accent(color) = rule.effect() {
            // Lower-ranked claims go in first so the cascade orders them;
            // the winner must still be the user rule color.
            cascade
                .set(
                    StyleOrigin::for_rule_origin(rule.origin()),
                    "content.accent",
                    color,
                )
                .expect("rule accent joins the cascade");
        }
    }
    let resolved = cascade.resolve("content.accent").expect("claimed");
    assert_eq!(resolved.origin, StyleOrigin::UserRule);
    assert_eq!(
        resolved.value,
        TokenColor::from_rgba([0x04, 0x05, 0x06, 0xFF])
    );
}

/// Chrome hosts tree nodes while rules place the panel: the overlay root
/// attachment and the placement rule resolve independently on one panel.
#[test]
fn chrome_hosts_nodes_while_rules_place_panels() {
    let mut chrome = WindowChromeRuntime::new();
    chrome.attach_overlay(UiNodeId::new(21)).expect("attaches");
    chrome
        .push_notification(
            ChromeNotification::new(
                NotificationId::new(1),
                "panel pinned",
                NotificationSeverity::Info,
            )
            .expect("fits"),
        )
        .expect("queues");
    chrome.open_command(UiNodeId::new(22)).expect("opens");
    assert!(chrome.is_visible(ChromeSurface::OverlayRoot));
    assert_eq!(chrome.overlay_nodes(), vec![UiNodeId::new(21)]);
    assert_eq!(chrome.command_target(), Some(UiNodeId::new(22)));

    let mut rules = PanelRuleSet::new();
    rules
        .add(PanelRule::new(
            RuleId::new(1),
            RuleOrigin::User,
            RuleSelector::Panel(PanelId::new(9)),
            RuleEffect::Placement(SceneLayer::Pinned),
        ))
        .expect("fits");
    rules
        .add(PanelRule::new(
            RuleId::new(2),
            RuleOrigin::User,
            RuleSelector::Panel(PanelId::new(9)),
            RuleEffect::Presentation(PresentationMode::Floating),
        ))
        .expect("fits");
    let placement = rules
        .resolve(PanelId::new(9), PanelType::Helper, EffectKind::Placement)
        .expect("resolves")
        .expect("claimed");
    assert_eq!(
        placement.effect(),
        RuleEffect::Placement(SceneLayer::Pinned)
    );

    // Closing the command surface does not disturb the overlay membership.
    chrome.close_command().expect("closes");
    assert_eq!(chrome.overlay_nodes(), vec![UiNodeId::new(21)]);
}

/// Conflicts surface as named diagnostics, and chrome reports its own
/// capacity failures with the same fail-closed vocabulary.
#[test]
fn conflicts_and_capacity_fail_closed_with_diagnostics() {
    let rules = PanelRuleSet::from_rules(vec![
        PanelRule::new(
            RuleId::new(3),
            RuleOrigin::Workspace,
            RuleSelector::PanelType(PanelType::Rich),
            RuleEffect::Presentation(PresentationMode::Floating),
        ),
        PanelRule::new(
            RuleId::new(4),
            RuleOrigin::Workspace,
            RuleSelector::PanelType(PanelType::Rich),
            RuleEffect::Presentation(PresentationMode::Tiled),
        ),
    ])
    .expect("fits");
    let err = rules
        .resolve(PanelId::new(1), PanelType::Rich, EffectKind::Presentation)
        .expect_err("tie fails closed");
    let diagnostic = match err {
        RuleError::Conflict(diagnostic) => diagnostic,
        other => panic!("expected conflict, got {other:?}"),
    };
    assert_eq!(
        diagnostic,
        RuleDiagnostic::new(
            EffectKind::Presentation,
            vec![RuleId::new(3), RuleId::new(4)]
        )
    );

    let mut chrome = WindowChromeRuntime::new();
    for n in 0..16u64 {
        chrome
            .push_notification(
                ChromeNotification::new(NotificationId::new(n), "fill", NotificationSeverity::Info)
                    .expect("fits"),
            )
            .expect("fits");
    }
    assert!(matches!(
        chrome.push_notification(
            ChromeNotification::new(NotificationId::new(99), "over", NotificationSeverity::Error)
                .expect("fits")
        ),
        Err(ChromeError::NotificationsFull { .. })
    ));
}
