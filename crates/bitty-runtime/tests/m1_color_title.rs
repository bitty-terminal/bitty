#![forbid(unsafe_code)]
//! M1-09 runtime color/title snapshot evidence (issue #1135, CTX-0571).
//!
//! Locks the runtime half of the Compatibility Milestone RFC color/title
//! acceptance evidence: "Snapshot tests for SGR 0–255 and truecolor cell
//! attributes; automated test for OSC 10/11 query-response round trip"
//! (`docs/specifications/compatibility-milestone-rfc.md`).
//!
//! Three claims, all through the headless `handle_pty_bytes ->
//! take_replies / tick -> headless_rgba` path `bitty --headless` uses:
//!
//! - OSC 10/11 query replies carry the exact xterm `rgb:RRRR/GGGG/BBBB` form
//!   for the active resolved palette (the earlier M1-04 suite pinned the
//!   round trip and the capability gate; this suite pins the *default*
//!   palette/theme application as a golden);
//! - OSC 0/2 titles reach the terminal state and the cold-event handoff
//!   unchanged;
//! - the default (unthemed) presentation is a stable frame digest, so a
//!   future change to default colors or title rendering is caught here.
//!
//! Oracle: accepted `compatibility-milestone-rfc.md` and
//! `terminal-state-rfc.md`. The grid-level goldens live in
//! `crates/bitty-compat-lab/tests/m1_color_golden.rs`; this file is the
//! runtime/presentation leg.

use bitty_runtime::{AnimationPolicy, ColdEvent, PresentStats, Runtime, RuntimeConfig};

/// Bitty Dark theme defaults (the resolved palette before any override).
const THEME_FG: [u8; 4] = [0xCD, 0xD6, 0xF4, 0xFF];
const THEME_BG: [u8; 4] = [0x1E, 0x1E, 0x2E, 0xFF];

/// The 16 base/bright ANSI entries of the Bitty Dark preset, pinned as the
/// default palette the runtime resolves and answers `OSC 4` queries from.
const DEFAULT_ANSI: [[u8; 3]; 16] = [
    [0x45, 0x47, 0x5A],
    [0xF3, 0x8B, 0xA8],
    [0xA6, 0xE3, 0xA1],
    [0xF9, 0xE2, 0xAF],
    [0x89, 0xB4, 0xFA],
    [0xF5, 0xC2, 0xE7],
    [0x94, 0xE2, 0xD5],
    [0xBA, 0xC2, 0xDE],
    [0x58, 0x5B, 0x70],
    [0xF3, 0x8B, 0xA8],
    [0xA6, 0xE3, 0xA1],
    [0xF9, 0xE2, 0xAF],
    [0x89, 0xB4, 0xFA],
    [0xF5, 0xC2, 0xE7],
    [0x94, 0xE2, 0xD5],
    [0xCD, 0xD6, 0xF4],
];

fn themed() -> Runtime {
    Runtime::new(RuntimeConfig {
        theme_resolved: true,
        ..RuntimeConfig::default()
    })
    .expect("themed runtime must build")
}

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

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn digest(rt: &Runtime, stats: &PresentStats) -> u64 {
    let mut bytes = Vec::with_capacity(128);
    for value in [
        stats.frame,
        stats.fills as u64,
        stats.rounded_fills as u64,
        stats.glyphs as u64,
        stats.cells_examined,
        stats.glyphs_emitted,
        stats.generation,
        stats.images as u64,
        stats.backgrounds as u64,
        stats.images_skipped as u64,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.push(u8::from(stats.headless));
    match rt.headless_rgba() {
        Some(rgba) => {
            bytes.push(1);
            bytes.extend_from_slice(&rgba);
        }
        None => bytes.push(0),
    }
    fnv1a(&bytes)
}

/// The default resolved theme answers OSC 10/11 queries with the exact xterm
/// `rgb:` bytes; the default palette is the Bitty Dark 16-color table.
#[test]
fn default_palette_and_theme_application_are_stable() {
    let mut rt = themed();
    assert_eq!(rt.active_foreground(), THEME_FG, "default foreground");
    assert_eq!(rt.active_background(), THEME_BG, "default background");
    for (index, rgb) in DEFAULT_ANSI.iter().enumerate() {
        assert_eq!(
            rt.active_palette_color(index as u8),
            *rgb,
            "ANSI {index} default palette entry"
        );
    }
    // Cube and ramp stay xterm-compatible beyond the preset's 16 entries.
    assert_eq!(rt.active_palette_color(16), [0, 0, 0]);
    assert_eq!(rt.active_palette_color(231), [255, 255, 255]);
    assert_eq!(rt.active_palette_color(232), [8, 8, 8]);
    assert_eq!(rt.active_palette_color(255), [238, 238, 238]);

    rt.handle_pty_bytes(b"\x1b]10;?\x07\x1b]11;?\x07");
    assert_eq!(
        replies_text(&mut rt),
        vec![
            b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\".to_vec(),
            b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\".to_vec(),
        ],
        "default-theme query replies use the xterm ST-terminated rgb form"
    );
}

/// OSC 0/2 titles reach terminal state and the cold-event handoff unchanged;
/// the later OSC 2 wins and a query of the live title matches.
#[test]
fn osc_title_reaches_state_and_cold_handoff() {
    let mut rt = themed();
    rt.handle_pty_bytes(b"\x1b]0;bitty-title-0\x1b\\\x1b]2;bitty-title-2\x07");
    assert_eq!(rt.snapshot().title.as_str(), "bitty-title-2");
    let cold = rt.drain_cold_events();
    let titles: Vec<String> = cold
        .iter()
        .filter_map(|ev| match ev {
            ColdEvent::TitleChanged(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        titles,
        vec!["bitty-title-0".to_string(), "bitty-title-2".to_string()],
        "both OSC 0 and OSC 2 titles cross the cold-event boundary in order"
    );
    // Empty OSC 0 resets the live title.
    rt.handle_pty_bytes(b"\x1b]0;\x07");
    assert_eq!(rt.snapshot().title.as_str(), "");
}

/// The default themed, deterministic first frame is pinned by digest, so the
/// default palette/theme application and the title path are regression-locked
/// at the presentation boundary.
#[test]
fn golden_default_theme_first_frame_digest() {
    let mut rt = themed_deterministic();
    let first = rt.tick().expect("first themed frame presents");
    let actual = digest(&rt, &first);
    assert_eq!(
        actual, 0xf194_7117_d9e4_06d1,
        "default themed frame digest changed (actual 0x{actual:016x})"
    );
    let rgba = rt.headless_rgba().expect("rgba");
    assert_eq!(
        &rgba[0..4],
        &THEME_BG,
        "the default background paints the surface corner"
    );
    assert!(rt.tick().is_none(), "no damage -> idle");
}
