//! Theme token contract tests (CTX-0612, UX-40).
//!
//! Candidate-status honesty: these tests pin the **draft candidate**
//! resolution order, plugin boundary, attribution, and contrast behavior from
//! the theme-token-contract candidate record. They compose with — and assert
//! nothing beyond — the accepted RFC-0001 grammar, AC floors, and `--safe`
//! posture.
//!
//! Headless, deterministic, and bounded: no window, no GPU, no PTY, no
//! filesystem.

#![forbid(unsafe_code)]

use bitty_ui::theme::{
    AC1_FOCUSED_VS_BACKGROUND_MIN, AC2_FOCUSED_VS_IDLE_MIN, BORDER_FOCUSED, BORDER_IDLE,
    CONTENT_BACKGROUND, CORE_TOKENS, PLUGIN_TOKEN_FALLBACK, SAFE_BORDER_FOCUSED, SAFE_BORDER_IDLE,
    TokenColor, TokenError, TokenLayer, check_plugin_keys, contrast_ratio, degrade_plugin_token,
    framework_default, is_terminal_key, resolve, resolve_validated, validate_contrast,
};

fn layer_of(name: &str, layer: TokenLayer) -> bool {
    resolve(&[], None, &[], false)
        .expect("defaults resolve")
        .layer_of(name)
        == Some(layer)
        || resolve(&[(name, "#112233")], None, &[], false)
            .ok()
            .and_then(|t| t.layer_of(name))
            == Some(layer)
}

#[test]
fn core_inventory_is_closed_and_has_defaults() {
    assert_eq!(CORE_TOKENS.len(), 17, "candidate inventory is 17 tokens");
    for name in CORE_TOKENS {
        assert!(
            framework_default(name).is_some(),
            "every Core token needs a framework default: {name}"
        );
        assert!(
            layer_of(name, TokenLayer::FrameworkDefault),
            "{name} resolves from the framework default"
        );
    }
    // A plugin may not invent a Core token under a closed namespace.
    for invented in [
        "chrome.status.background",
        "content.glow",
        "state.info",
        "border.glow",
    ] {
        let err = resolve(&[(invented, "#112233")], None, &[], false)
            .expect_err("invented Core token must fail");
        assert!(
            matches!(err, TokenError::UnknownToken { .. }),
            "expected UnknownToken for {invented}, got {err}"
        );
        assert!(
            err.to_string().contains(invented),
            "diagnostic must name the key: {err}"
        );
    }
}

#[test]
fn resolution_order_later_wins_with_attribution() {
    let tokens = resolve(
        &[("content.accent", "#111111")],
        Some(("notes", &[("plugin.notes.pin", "#222222")])),
        &[("content.accent", "#333333")],
        false,
    )
    .expect("layers resolve");
    let accent = tokens.get("content.accent").expect("accent resolves");
    assert_eq!(accent.value, TokenColor::parse("#333333").unwrap());
    assert_eq!(accent.layer, TokenLayer::UserKeys);
    assert_eq!(
        tokens.layer_of("content.accent"),
        Some(TokenLayer::UserKeys)
    );
    // Untouched keys keep earlier winners.
    assert_eq!(
        tokens.layer_of("chrome.bar.background"),
        Some(TokenLayer::FrameworkDefault)
    );
    assert_eq!(
        tokens.get("plugin.notes.pin").expect("plugin token").layer,
        TokenLayer::PluginPackage
    );
    // Theme preset alone wins over the default with preset attribution.
    let preset =
        resolve(&[("content.accent", "#444444")], None, &[], false).expect("preset resolves");
    assert_eq!(
        preset.layer_of("content.accent"),
        Some(TokenLayer::ThemePreset)
    );
}

#[test]
fn safe_forces_outline_pair_and_ignores_user_and_preset() {
    let (tokens, _) = resolve_validated(
        &[(BORDER_FOCUSED, "#112233"), (BORDER_IDLE, "#112233")],
        None,
        &[(BORDER_FOCUSED, "#445566"), (BORDER_IDLE, "#445566")],
        true,
    )
    .expect("safe mode resolves a compliant pair");
    assert_eq!(
        tokens.get(BORDER_FOCUSED).unwrap().value,
        SAFE_BORDER_FOCUSED
    );
    assert_eq!(tokens.get(BORDER_IDLE).unwrap().value, SAFE_BORDER_IDLE);
    assert_eq!(
        tokens.layer_of(BORDER_FOCUSED),
        Some(TokenLayer::SafeForced)
    );
    assert_eq!(tokens.layer_of(BORDER_IDLE), Some(TokenLayer::SafeForced));
    // Keys outside the governed pair are unaffected by --safe.
    let tokens = resolve(
        &[("content.accent", "#123456")],
        None,
        &[("content.accent", "#654321")],
        true,
    )
    .expect("safe resolves");
    assert_eq!(
        tokens.get("content.accent").unwrap().value,
        TokenColor::parse("#654321").unwrap()
    );
    assert_eq!(
        tokens.layer_of("content.accent"),
        Some(TokenLayer::UserKeys)
    );
}

#[test]
fn plugin_package_is_rejected_for_chrome_and_borders() {
    // A plugin theme package claiming chrome is rejected fail-closed.
    let err = resolve(
        &[],
        Some(("notes", &[("chrome.bar.background", "#112233")])),
        &[],
        false,
    )
    .expect_err("plugin chrome claim must fail");
    assert!(
        matches!(err, TokenError::OutOfNamespace { .. }),
        "expected OutOfNamespace, got {err}"
    );
    assert!(err.to_string().contains("chrome.bar.background"));

    // Same for borders, content, and state namespaces.
    for key in [
        "border.focused",
        "content.accent",
        "state.error",
        "terminal.cursor",
    ] {
        let err = check_plugin_keys("notes", &[(key, "#112233")])
            .expect_err("out-of-namespace plugin key must fail");
        assert!(
            !matches!(err, TokenError::UnknownToken { .. }),
            "{key} must fail as a boundary violation, got {err}"
        );
    }

    // A valid own-namespace package resolves and never moves Core chrome.
    let before = resolve(&[], None, &[], false).expect("defaults resolve");
    let after = resolve(
        &[],
        Some(("notes", &[("plugin.notes.pin", "#A6E3A1")])),
        &[],
        false,
    )
    .expect("own-namespace package resolves");
    for name in CORE_TOKENS {
        assert_eq!(
            before.get(name).unwrap().value,
            after.get(name).unwrap().value,
            "plugin package must not move Core token {name}"
        );
        assert_eq!(after.layer_of(name), Some(TokenLayer::FrameworkDefault));
    }
    assert_eq!(
        after.layer_of("plugin.notes.pin"),
        Some(TokenLayer::PluginPackage)
    );
}

#[test]
fn plugin_tokens_rejected_for_other_plugin_namespaces() {
    let err = check_plugin_keys("notes", &[("plugin.tasks.pin", "#112233")])
        .expect_err("cross-plugin claim must fail");
    match &err {
        TokenError::OutOfNamespace { key, plugin } => {
            assert_eq!(key, "plugin.tasks.pin");
            assert_eq!(plugin, "notes");
        }
        other => panic!("expected OutOfNamespace, got {other}"),
    }
    // A well-formed own-namespace package passes the boundary check.
    check_plugin_keys("notes", &[("plugin.notes.pin", "#A6E3A1")]).expect("own namespace passes");
}

#[test]
fn duplicate_key_in_one_layer_fails_closed() {
    let err = resolve(
        &[("content.accent", "#111111"), ("content.accent", "#222222")],
        None,
        &[],
        false,
    )
    .expect_err("preset duplicate must fail");
    assert!(
        matches!(
            err,
            TokenError::DuplicateKey {
                layer: TokenLayer::ThemePreset,
                ..
            }
        ),
        "expected preset DuplicateKey, got {err}"
    );
    assert!(err.to_string().contains("content.accent"));

    let err = resolve(
        &[],
        None,
        &[("state.error", "#111111"), ("state.error", "#222222")],
        false,
    )
    .expect_err("user duplicate must fail");
    assert!(
        matches!(
            err,
            TokenError::DuplicateKey {
                layer: TokenLayer::UserKeys,
                ..
            }
        ),
        "expected user DuplicateKey, got {err}"
    );
}

#[test]
fn malformed_values_fail_closed_naming_the_key() {
    for raw in ["red", "#RGB", "#12345", "#1234567", "123456", "#GGGGGG", ""] {
        let err = resolve(&[("content.accent", raw)], None, &[], false)
            .expect_err("malformed value must fail");
        match &err {
            TokenError::BadValue { key, raw: got, .. } => {
                assert_eq!(key, "content.accent");
                assert_eq!(got, raw);
            }
            other => panic!("expected BadValue for '{raw}', got {other}"),
        }
        assert!(err.to_string().contains("content.accent"));
    }
    // The accepted grammar spellings parse.
    assert!(TokenColor::parse("#112233").is_some());
    assert!(TokenColor::parse("#112233AA").is_some());
    assert_eq!(
        TokenColor::parse("#112233").unwrap().to_hex(),
        "#112233".to_string()
    );
}

#[test]
fn violating_preset_pair_is_rejected_with_key_and_values() {
    // AC-1: focused invisible against the background.
    let err = resolve_validated(&[(BORDER_FOCUSED, "#1E1E2E")], None, &[], false)
        .expect_err("AC-1 violation must fail");
    match &err {
        TokenError::Contrast { rule, detail } => {
            assert_eq!(*rule, "AC-1");
            assert!(
                detail.contains(BORDER_FOCUSED),
                "detail names key: {detail}"
            );
            assert!(detail.contains("#1E1E2E"), "detail names value: {detail}");
        }
        other => panic!("expected AC-1 Contrast, got {other}"),
    }

    // AC-2: focused distinct from the background but too close to idle.
    // `#6E6E6E` clears AC-1 (~3.2:1 vs the background) yet sits near the
    // idle composite, so only AC-2 fires.
    let err = resolve_validated(&[(BORDER_FOCUSED, "#6E6E6E")], None, &[], false)
        .expect_err("AC-2 violation must fail");
    assert!(
        matches!(err, TokenError::Contrast { rule: "AC-2", .. }),
        "expected AC-2 Contrast, got {err}"
    );

    // A compliant pair passes with floors intact.
    let ratio_focused_bg = {
        let tokens = resolve(&[], None, &[], false).expect("defaults");
        let bg = tokens.get(CONTENT_BACKGROUND).unwrap().value.rgb();
        let focused = tokens.get(BORDER_FOCUSED).unwrap().value;
        let idle = tokens.get(BORDER_IDLE).unwrap().value;
        let ac1 = contrast_ratio(focused.composited_over(bg), bg);
        let ac2 = contrast_ratio(focused.composited_over(bg), idle.composited_over(bg));
        assert!(
            ac1 >= AC1_FOCUSED_VS_BACKGROUND_MIN,
            "framework defaults must clear AC-1: {ac1:.2}:1"
        );
        assert!(
            ac2 >= AC2_FOCUSED_VS_IDLE_MIN,
            "framework defaults must clear AC-2: {ac2:.2}:1"
        );
        ac1
    };
    assert!(ratio_focused_bg >= 3.0);
    let (_, notes) = resolve_validated(&[], None, &[], false).expect("defaults pass");
    assert!(
        notes.is_empty(),
        "framework defaults clear even advisory AC-3: {notes:?}"
    );
}

#[test]
fn ac3_is_advisory_never_fail_closed() {
    // Idle barely visible against the background but distinct from focused:
    // AC-1 and AC-2 hold while AC-3 trips, so resolution succeeds with a note.
    let tokens = resolve(&[(BORDER_IDLE, "#2A2A3A")], None, &[], false)
        .expect("low-idle preset layers without failing");
    let notes = validate_contrast(&tokens).expect("AC-3 must not fail");
    assert_eq!(notes.len(), 1, "exactly one advisory note expected");
    assert_eq!(notes[0].rule, "AC-3");
    assert!(notes[0].detail.contains(BORDER_IDLE));
}

#[test]
fn terminal_truth_is_never_tokenized() {
    assert!(is_terminal_key("terminal.cursor"));
    assert!(is_terminal_key("terminal"));
    for key in ["terminal.cursor", "terminal.palette.1"] {
        for layer in [TokenLayer::ThemePreset, TokenLayer::UserKeys] {
            let err = if layer == TokenLayer::ThemePreset {
                resolve(&[(key, "#112233")], None, &[], false).expect_err("terminal key must fail")
            } else {
                resolve(&[], None, &[(key, "#112233")], false).expect_err("terminal key must fail")
            };
            assert!(
                matches!(err, TokenError::TerminalNamespace { .. }),
                "expected TerminalNamespace for {key}, got {err}"
            );
        }
        assert!(
            check_plugin_keys("notes", &[(key, "#112233")]).is_err(),
            "plugin layer must also reject {key}"
        );
    }
    // The resolved set structurally contains no terminal keys.
    let tokens = resolve(
        &[("content.accent", "#123456")],
        Some(("notes", &[("plugin.notes.pin", "#A6E3A1")])),
        &[(BORDER_FOCUSED, "#33CCFF")],
        true,
    )
    .expect("full layers resolve");
    for (name, _) in tokens.iter() {
        assert!(
            !is_terminal_key(name),
            "resolved set must never carry terminal keys: {name}"
        );
    }
}

#[test]
fn plugin_contrast_violation_degrades_instead_of_failing() {
    let tokens = resolve(&[], None, &[], false).expect("defaults");
    let background = tokens.get(CONTENT_BACKGROUND).unwrap().value;
    // Near-invisible plugin token on the panel background.
    let weak = TokenColor::parse("#202030").unwrap();
    let (value, note) = degrade_plugin_token("plugin.notes.pin", weak, background);
    assert_eq!(value, PLUGIN_TOKEN_FALLBACK);
    let note = note.expect("violation is reported");
    assert_eq!(note.rule, "AC-3");
    assert!(note.detail.contains("plugin.notes.pin"));
    // A legible plugin token passes through untouched.
    let strong = TokenColor::parse("#F5E0DC").unwrap();
    let (value, note) = degrade_plugin_token("plugin.notes.pin", strong, background);
    assert_eq!(value, strong);
    assert!(note.is_none());
}
