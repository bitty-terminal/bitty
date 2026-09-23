//! M1-04 acceptance evidence: OSC 10/11 foreground/background color
//! query and set round trip.
//!
//! Issue #1129. The mechanism landed in `bitty` `ef73c82` (CTX-0381): the VT
//! parser classifies and bounds `OSC 10`/`OSC 11` into
//! `TerminalAction::OscDynamicColor` (fail-closed parsing), the runtime
//! answers queries from the resolved theme palette in the standard xterm
//! `rgb:RRRR/GGGG/BBBB` form once the theme resolved, and applies sets only
//! when the embedder grants the default-deny `osc_color_set_allowed`
//! capability. The oracle is the accepted `compatibility-milestone-rfc.md`
//! M1 row "Window title | OSC 0 and OSC 2 (set); OSC 10/11 query + set |
//! Required" (`docs/` submodule, `specifications/`) and its acceptance
//! evidence line "automated test for OSC 10/11 query-response round trip".
//!
//! These tests lock the claims the issue names — query reply byte shape,
//! set-then-query round trip, and default-deny/embedder-granted behavior —
//! through the headless `handle_pty_bytes -> take_replies -> active_*` path
//! plus a pixel-level repaint assertion for a granted background set.
#![forbid(unsafe_code)]

use bitty_runtime::{AnimationPolicy, Runtime, RuntimeConfig};

/// Bitty Dark theme defaults (the `theme_resolved` palette the runtime
/// answers from before any override).
const THEME_FG: [u8; 4] = [0xCD, 0xD6, 0xF4, 0xFF];
const THEME_BG: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];

/// Runtime carrying a user-resolved palette: OSC 10/11 queries are answered
/// only once the theme resolved, so evidence tests use this constructor.
fn themed() -> Runtime {
    Runtime::new(RuntimeConfig {
        theme_resolved: true,
        ..RuntimeConfig::default()
    })
    .expect("themed runtime must build")
}

/// Deterministic rasterizer variant of [`themed`] for the pixel assertion.
fn themed_deterministic() -> Runtime {
    Runtime::with_deterministic_rasterizer(RuntimeConfig {
        animations: AnimationPolicy {
            enabled: false,
            ..AnimationPolicy::default()
        },
        theme_resolved: true,
        ..RuntimeConfig::default()
    })
    .expect("deterministic themed runtime must build")
}

fn replies_text(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies().iter().map(|b| b.to_vec()).collect()
}

/// Query reply byte shape: exact xterm `OSC <10|11>;rgb:RRRR/GGGG/BBBB ST`
/// bytes derived from the active resolved palette.
#[test]
fn osc10_osc11_query_reply_bytes_use_xterm_rgb_form() {
    let mut rt = themed();
    rt.handle_pty_bytes(b"\x1b]10;?\x07");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\".to_vec()],
        "foreground reply duplicates each 8-bit channel and uses ST"
    );
    rt.handle_pty_bytes(b"\x1b]11;?\x1b\\");
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\".to_vec()],
        "background reply uses ST terminator too"
    );
}

/// Set-then-query round trip: after the embedder grants the capability, a
/// set changes the active color and a following query reports the override in
/// the same `rgb:` shape, for both targets and both accepted input forms
/// (`#RRGGBB` and the short `#RGB` form).
#[test]
fn osc10_osc11_set_then_query_round_trips() {
    let mut rt = themed();
    rt.set_osc_color_set_allowed(true);
    rt.handle_pty_bytes(b"\x1b]10;#112233\x07\x1b]11;rgb:44/55/66\x07");
    assert!(
        replies_text(&mut rt).is_empty(),
        "sets alone produce no reply"
    );
    assert_eq!(rt.active_foreground(), [0x11, 0x22, 0x33, 0xFF]);
    assert_eq!(rt.active_background(), [0x44, 0x55, 0x66, 0xFF]);
    rt.handle_pty_bytes(b"\x1b]10;?\x07\x1b]11;?\x07");
    assert_eq!(
        replies_text(&mut rt),
        vec![
            b"\x1b]10;rgb:1111/2222/3333\x1b\\".to_vec(),
            b"\x1b]11;rgb:4444/5555/6666\x1b\\".to_vec(),
        ],
        "queries report the live override, not the base theme"
    );
    // Short hex form expands per xterm scaling (#abc -> aa/bb/cc).
    rt.handle_pty_bytes(b"\x1b]10;#abc\x07\x1b]10;?\x07");
    assert_eq!(rt.active_foreground(), [0xAA, 0xBB, 0xCC, 0xFF]);
    assert_eq!(
        replies_text(&mut rt),
        vec![b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\".to_vec()]
    );
}

/// Default-deny: with no embedder grant, an untrusted set is inert — the
/// active color is unchanged, no reply is queued, and the presented pixels
/// are byte-identical to the pre-set baseline (the denied set must not even
/// force a repaint with different output).
#[test]
fn osc10_osc11_set_is_default_deny_and_pixel_inert() {
    let mut rt = themed_deterministic();
    assert!(!rt.osc_color_set_allowed(), "deny by default");
    assert!(rt.tick().is_some(), "baseline full redraw");
    assert!(rt.tick().is_none(), "idle at baseline");
    let baseline = rt.headless_rgba().expect("baseline rgba");
    rt.handle_pty_bytes(b"\x1b]10;#112233\x07\x1b]11;rgb:44/55/66\x07");
    assert!(replies_text(&mut rt).is_empty(), "denied sets stay silent");
    assert_eq!(rt.active_foreground(), THEME_FG, "foreground unchanged");
    assert_eq!(rt.active_background(), THEME_BG, "background unchanged");
    let _ = rt.tick();
    assert_eq!(
        rt.headless_rgba().expect("rgba after denied set"),
        baseline,
        "a denied set leaves the presented pixels byte-identical"
    );
}

/// Embedder-granted: once `set_osc_color_set_allowed(true)` is called, a
/// background set repaints the surface — the presented pixels change and the
/// first non-glyph pixel carries the new background — proving the override
/// reaches the render path, not just the query state.
#[test]
fn granted_osc11_set_repaints_the_surface() {
    let mut rt = themed_deterministic();
    // #1349: the subject here is background-override plumbing, so the
    // default-on bar row is hidden to keep the `glyphs == 0` pin exact (a
    // full re-render with chrome would legitimately re-emit bar glyphs).
    rt.set_workspaceline_visible(false);
    assert!(rt.tick().is_some(), "baseline full redraw");
    let baseline = rt.headless_rgba().expect("baseline rgba");
    assert_eq!(
        &baseline[0..4],
        &THEME_BG,
        "default background paints the surface corner"
    );
    rt.set_osc_color_set_allowed(true);
    rt.handle_pty_bytes(b"\x1b]11;#102030\x07");
    let repaint = rt.tick().expect("a granted set forces a repaint");
    assert!(repaint.glyphs == 0, "a color-only change adds no glyphs");
    let after = rt.headless_rgba().expect("rgba after granted set");
    assert_ne!(after, baseline, "the repaint changes presented pixels");
    assert_eq!(
        &after[0..4],
        &[0x10, 0x20, 0x30, 0xFF],
        "the surface corner now carries the granted background"
    );
    assert_eq!(rt.active_background(), [0x10, 0x20, 0x30, 0xFF]);
}

/// Malformed OSC 10/11 payloads fail closed: no query reply, no set, and no
/// change to the active colors — untrusted bytes can never smuggle a partial
/// write through the fail-closed parser.
#[test]
fn osc10_osc11_malformed_payloads_fail_closed() {
    let mut rt = themed();
    rt.set_osc_color_set_allowed(true);
    for sequence in [
        &b"\x1b]10;#zzzzzz\x07"[..],
        &b"\x1b]10;#12345\x07"[..],
        &b"\x1b]11;rgb:1/2\x07"[..],
        &b"\x1b]11;rgb:1/2/3/4\x07"[..],
        &b"\x1b]10;not-a-color\x07"[..],
        &b"\x1b]11;\x07"[..],
    ] {
        rt.handle_pty_bytes(sequence);
    }
    assert!(
        replies_text(&mut rt).is_empty(),
        "malformed payloads never query or reply"
    );
    assert_eq!(rt.active_foreground(), THEME_FG);
    assert_eq!(rt.active_background(), THEME_BG);
}
