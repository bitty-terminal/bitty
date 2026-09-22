//! U-7 seven panel state axes + declarative rehydration (UX-26/UX-27,
//! CTX-0672).
//!
//! Candidate behavior: the seven axes live in `panel_state`, snapshots and
//! fresh-VM re-application live in `panel_rehydrate`. Cross-module checks
//! that the unit tests inside those modules do not cover alone: full
//! seven-axis round-trips, snapshot rehydration of several panels,
//! unknown-field tolerance, budget caps, and version gating.

#![forbid(unsafe_code)]

use bitty_ui::{
    MAX_BADGE_COUNT, MAX_BADGE_TEXT_LEN, MAX_RECORD_LEN, MAX_SNAPSHOT_PANELS, PanelActivity,
    PanelAttention, PanelFocusState, PanelId, PanelInteraction, PanelLifecycle, PanelRecord,
    PanelStateError, PanelVisibility, PresentationMode, RehydrateError, SevenPanelState, UiNodeId,
    encode_snapshot, rehydrate_snapshot,
};

fn mounted(panel: u64, node: u64) -> SevenPanelState {
    let mut s = SevenPanelState::new(PanelId::new(panel), UiNodeId::new(node));
    s.set_lifecycle(PanelLifecycle::Created).unwrap();
    s.set_lifecycle(PanelLifecycle::Mounted).unwrap();
    s.set_visibility(PanelVisibility::Visible);
    s
}

// ---------------------------------------------------------------------------
// UX-26: close never unloads the plugin
// ---------------------------------------------------------------------------

#[test]
fn close_parks_panel_but_retains_plugin() {
    let mut s = mounted(1, 101);
    s.focus_panel().unwrap();
    s.set_attention(PanelAttention::badge(3, "msgs").unwrap());
    let out = s.close().unwrap();
    assert!(out.plugin_retained, "close must never unload the plugin");
    assert!(out.reopenable);
    assert_eq!(s.lifecycle(), PanelLifecycle::Closed);
    assert_eq!(s.visibility(), PanelVisibility::Hidden);
    assert_eq!(s.focus(), PanelFocusState::Unfocused);
    // Attention survives close: badges still count on a closed panel.
    assert_eq!(s.attention(), &PanelAttention::badge(3, "msgs").unwrap());
    // Only the explicit dispose releases the plugin.
    s.reopen().unwrap();
    assert_eq!(s.lifecycle(), PanelLifecycle::Mounted);
    s.set_lifecycle(PanelLifecycle::Disposed).unwrap();
    assert_eq!(s.close(), Err(PanelStateError::AlreadyDisposed));
}

// ---------------------------------------------------------------------------
// UX-26/UX-27: seven-axis round-trip through a record
// ---------------------------------------------------------------------------

#[test]
fn all_seven_axes_round_trip_through_record() {
    let mut s = mounted(7, 707);
    s.focus_panel().unwrap();
    s.set_presentation(PresentationMode::Floating);
    s.set_attention(PanelAttention::badge(42, "msgs").unwrap());
    s.set_interaction(PanelInteraction::Typing);
    s.set_activity(PanelActivity::Active);
    let line = PanelRecord::capture(&s).encode();
    let back = PanelRecord::decode(&line).unwrap();
    assert_eq!(back, PanelRecord::capture(&s));
    // A focused record replays through the gate to mounted and then
    // re-owns routing via the focus axis, ending focused on the fresh VM.
    let fresh = back.apply_fresh().unwrap();
    assert_eq!(fresh.lifecycle(), PanelLifecycle::Focused);
    assert_eq!(fresh.focus(), PanelFocusState::Focused);
    assert_eq!(fresh.presentation(), PresentationMode::Floating);
    assert_eq!(fresh.visibility(), PanelVisibility::Visible);
    assert_eq!(fresh.attention(), s.attention());
    assert_eq!(fresh.interaction(), PanelInteraction::Typing);
    assert_eq!(fresh.activity(), PanelActivity::Active);
    assert_eq!(fresh.panel(), PanelId::new(7));
    assert_eq!(fresh.node(), UiNodeId::new(707));
}

// ---------------------------------------------------------------------------
// UX-27: rehydrate several panels from one snapshot document
// ---------------------------------------------------------------------------

#[test]
fn rehydrate_snapshot_applies_fresh_states_sorted() {
    let mut a = mounted(30, 130);
    a.set_attention(PanelAttention::Urgent);
    a.set_activity(PanelActivity::Active);
    let mut b = mounted(10, 110);
    b.close().unwrap();
    let c = SevenPanelState::new(PanelId::new(20), UiNodeId::new(120));
    let doc = encode_snapshot(&[a, b, c]);
    let report = rehydrate_snapshot(&doc).unwrap();
    assert_eq!(report.applied, vec![10, 20, 30]);
    assert_eq!(report.states.len(), 3);
    let closed = report.get(PanelId::new(10)).unwrap();
    assert_eq!(closed.lifecycle(), PanelLifecycle::Closed);
    let urgent = report.get(PanelId::new(30)).unwrap();
    assert_eq!(urgent.attention(), &PanelAttention::Urgent);
    assert_eq!(urgent.activity(), PanelActivity::Active);
    let parked = report.get(PanelId::new(20)).unwrap();
    assert_eq!(parked.lifecycle(), PanelLifecycle::Declared);
}

// ---------------------------------------------------------------------------
// UX-27: unknown-field tolerance (forward compatibility)
// ---------------------------------------------------------------------------

#[test]
fn unknown_fields_comments_and_blanks_are_tolerated() {
    let mut s = mounted(5, 105);
    s.set_attention(PanelAttention::badge(9, "a;b=c\\d\ne").unwrap());
    let mut doc = String::from("# u7 snapshot v1\n\n");
    doc.push_str(&PanelRecord::capture(&s).encode());
    doc.push_str(";zzz_future=1;another_deep=x");
    doc.push_str("\n\n# trailing comment\n");
    let report = rehydrate_snapshot(&doc).unwrap();
    assert_eq!(report.applied, vec![5]);
    let back = report.get(PanelId::new(5)).unwrap();
    assert_eq!(back.attention(), s.attention());
    assert_eq!(back.lifecycle(), PanelLifecycle::Mounted);
}

// ---------------------------------------------------------------------------
// UX-27: budget caps fail closed
// ---------------------------------------------------------------------------

#[test]
fn snapshot_panel_budget_cap_rejects() {
    let states: Vec<SevenPanelState> = (0..MAX_SNAPSHOT_PANELS as u64 + 1)
        .map(|i| SevenPanelState::new(PanelId::new(i + 1), UiNodeId::new(i + 1)))
        .collect();
    let doc = encode_snapshot(&states);
    let err = rehydrate_snapshot(&doc).unwrap_err();
    assert_eq!(
        err,
        RehydrateError::TooManyPanels {
            found: MAX_SNAPSHOT_PANELS + 1,
            cap: MAX_SNAPSHOT_PANELS,
        }
    );
}

#[test]
fn record_length_budget_cap_rejects() {
    let line = format!("panel=1;pad={}", "x".repeat(MAX_RECORD_LEN));
    let err = rehydrate_snapshot(&line).unwrap_err();
    assert!(matches!(err, RehydrateError::RecordTooLong { line: 1, .. }));
}

#[test]
fn badge_text_budget_cap_rejects() {
    let long = "x".repeat(MAX_BADGE_TEXT_LEN + 1);
    assert_eq!(
        PanelAttention::badge(1, &long),
        Err(PanelStateError::BadgeTextTooLong {
            found: MAX_BADGE_TEXT_LEN + 1,
            cap: MAX_BADGE_TEXT_LEN,
        })
    );
    // A saturated count still stores raw and renders capped.
    let big = PanelAttention::badge(u32::MAX, "msgs").unwrap();
    assert_eq!(big.display_count(), Some(MAX_BADGE_COUNT));
    assert!(big.is_saturated());
    // An oversized badge inside a record fails the rehydration, not the build.
    let doc = format!("v=1;panel=1;node=1;attention=badge;attention_count=1;attention_text={long}");
    assert!(matches!(
        rehydrate_snapshot(&doc).unwrap_err(),
        RehydrateError::BadValue { .. }
    ));
}

// ---------------------------------------------------------------------------
// UX-27: version gating + malformed input fails closed, applies nothing
// ---------------------------------------------------------------------------

#[test]
fn unsupported_version_and_malformed_input_reject() {
    let good = PanelRecord::capture(&mounted(1, 101)).encode();
    let doctored = good.replacen("v=1;", "v=9;", 1);
    assert_eq!(
        rehydrate_snapshot(&doctored).unwrap_err(),
        RehydrateError::UnsupportedVersion {
            found: 9,
            supported: 1,
        }
    );
    // Bad axis value on line 2 rejects the whole document.
    let doc = format!("{good}\nv=1;panel=2;node=2;lifecycle=bogus");
    assert_eq!(
        rehydrate_snapshot(&doc).unwrap_err(),
        RehydrateError::BadValue {
            line: 2,
            key: "lifecycle".to_owned(),
        }
    );
    // Duplicate panels reject.
    let doc = format!("{good}\n{good}");
    assert_eq!(
        rehydrate_snapshot(&doc).unwrap_err(),
        RehydrateError::DuplicatePanel { panel: 1 }
    );
    // Missing identity rejects.
    assert_eq!(
        rehydrate_snapshot("v=1;node=2").unwrap_err(),
        RehydrateError::Malformed { line: 1 }
    );
}
