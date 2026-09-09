//! Help popup overlay tests (CTX-0265, 009 which-key).
//!
//! Headless proof over the public `Runtime` API (no display server):
//!
//! 1. Toggle flips visibility and forces exactly one repaint, then idles.
//! 2. `Esc` dismisses the popup and is consumed (zero PTY bytes); with no
//!    popup the same `Esc` encodes normally (no behavior theft).
//! 3. Overlay-only: while shown, `snapshot().cells` are byte-identical and
//!    only the presented fills/glyphs grow (never grid truth).
//! 4. Stored rows are bounded ([`HELP_MAX_ROWS`]).
//! 5. Rows derive from the live keymap registry (added chord listed,
//!    Super-flip re-spelling listed).

#![forbid(unsafe_code)]

use bitty_platform::{KeyEvent, KeyLocation, LogicalKey, NamedKey, PressState};
use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn esc_press() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn live_help_rows() -> Vec<String> {
    let maps = bitty_config::keymap::default_keymaps().expect("defaults valid");
    bitty_config::keymap::help_rows_from_keymaps(&maps)
}

#[test]
fn help_toggle_flips_and_repaints_once() {
    let mut rt = make_runtime();
    assert!(rt.tick().is_some(), "first tick presents");
    assert_eq!(rt.tick(), None, "idle before toggle");
    assert!(!rt.help_visible());

    rt.set_help_rows(live_help_rows());
    assert!(rt.toggle_help(), "toggle on returns visible");
    assert!(rt.help_visible());
    assert!(
        rt.tick().is_some(),
        "visibility flip forces one present with no PTY damage"
    );
    assert_eq!(rt.tick(), None, "overlay frame then idles");

    assert!(!rt.toggle_help(), "toggle off returns hidden");
    assert!(!rt.help_visible());
    assert!(rt.tick().is_some(), "dismissal repaints once");
    assert_eq!(rt.tick(), None, "then idles");
}

#[test]
fn help_esc_dismisses_and_is_consumed() {
    let mut rt = make_runtime();
    let _ = rt.tick();
    rt.set_help_rows(live_help_rows());
    assert!(rt.toggle_help());

    assert_eq!(
        rt.handle_key_event(esc_press()),
        None,
        "dismissal Esc is consumed"
    );
    assert!(!rt.help_visible(), "Esc dismissed the popup");
    assert!(
        rt.drain_pending_input().is_empty(),
        "dismissal Esc never reaches the PTY"
    );
}

#[test]
fn help_esc_without_popup_encodes_normally() {
    // No behavior theft: with nothing shown, `Esc` still encodes for the
    // shell (vim survives the help feature).
    let mut rt = make_runtime();
    let _ = rt.tick();
    assert!(!rt.help_visible());
    assert_eq!(rt.handle_key_event(esc_press()), Some(vec![27]));
    assert!(!rt.help_visible());
}

#[test]
fn help_overlay_never_touches_grid_truth() {
    // Overlay-only proof: grid cells are byte-identical while the popup
    // shows, and the presented frame still carries the panel (fills plus
    // glyphs grow versus the bare grid frame).
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"hello-grid");
    let bare = rt.tick().expect("grid damage presents");
    let cells_before = rt.snapshot().cells.to_vec();

    rt.set_help_rows(live_help_rows());
    assert!(rt.toggle_help());
    let overlaid = rt.tick().expect("overlay forces a present");
    let cells_after = rt.snapshot().cells.to_vec();
    assert_eq!(
        cells_before, cells_after,
        "overlay must not mutate grid cells"
    );
    assert!(
        overlaid.fills > bare.fills,
        "panel paints fills: {} -> {}",
        bare.fills,
        overlaid.fills
    );
    assert!(
        overlaid.glyphs > bare.glyphs,
        "panel paints glyphs: {} -> {}",
        bare.glyphs,
        overlaid.glyphs
    );
}

#[test]
fn help_rows_are_bounded() {
    let mut rt = make_runtime();
    let many: Vec<String> = (0..500).map(|i| format!("alt+f{i}  toggle_zoom")).collect();
    rt.set_help_rows(many);
    assert_eq!(rt.help_rows().len(), bitty_runtime::HELP_MAX_ROWS);
}

#[test]
fn help_rows_come_from_the_live_registry() {
    // End-to-end registry proof at the runtime seam: an added chord is
    // listed, and the Super flip re-spells the gesture.
    let effective = bitty_config::EffectiveConfig {
        keymaps: vec![bitty_config::KeymapEntry {
            chord: "alt+e".into(),
            action: "open_composer".into(),
            context: "global".into(),
        }],
        ..Default::default()
    };
    let maps = bitty_config::keymap::resolve_keymaps(&effective).expect("resolves");
    let rows = bitty_config::keymap::help_rows_from_keymaps(&maps);
    assert!(rows.iter().any(|r| r == "alt+e  open_composer"));

    let flipped = bitty_config::EffectiveConfig {
        mod_key: bitty_config::ModKey::Super,
        ..Default::default()
    };
    let maps = bitty_config::keymap::resolve_keymaps(&flipped).expect("resolves");
    let rows = bitty_config::keymap::help_rows_from_keymaps(&maps);
    assert!(rows.iter().any(|r| r == "super+`  toggle_help"));
    assert!(!rows.iter().any(|r| r.starts_with("alt+")));
}
