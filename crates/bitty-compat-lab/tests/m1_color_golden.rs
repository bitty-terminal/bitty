#![forbid(unsafe_code)]
//! M1-09 color/title golden snapshots — cell attributes and title (issue
//! #1135, CTX-0571).
//!
//! The accepted Compatibility Milestone RFC requires "Snapshot tests for
//! SGR 0–255 and truecolor cell attributes; automated test for OSC 10/11
//! query-response round trip" (`docs/specifications/compatibility-milestone-rfc.md`).
//! This suite locks the grid-level half: each fixture under
//! `tests/compat/color/corpus/` pins the canonical `State::state_hash`
//! (Terminal-state RFC replay guarantee 2) plus the resolved
//! foreground/background attributes and the window title.
//!
//! OSC 10/11 payloads are semantically inert for grid truth by contract
//! (`CTX-0381`): the runtime owns the active palette, query replies, and the
//! gated set path, so the query/set fixtures here assert the *inert* grid
//! hash, and the runtime reply bytes are pinned separately in
//! `crates/bitty-runtime/tests/m1_color_title.rs`.

use std::path::PathBuf;

use bitty_term_state::{Color, Rgb, State};
use bitty_vt::Parser;

/// Canonical-hash version this golden binds to (`CANONICAL_HASH_VERSION`).
const HASH_VERSION: u32 = 9;

fn corpus_dir() -> PathBuf {
    bitty_compat_lab::workspace_root().join("tests/compat/color/corpus")
}

fn state_of(name: &str) -> State {
    let path = corpus_dir().join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(&bytes, |a| actions.push(a));
    let mut state = State::new();
    for action in &actions {
        state.apply(action);
    }
    state
        .check_invariants()
        .unwrap_or_else(|e| panic!("{name}: invariant violation {e:?}"));
    state
}

fn assert_golden(name: &str, hash: u64, generation: u64, title: &str) {
    let state = state_of(name);
    assert_eq!(
        state.state_hash(),
        hash,
        "{name}: canonical state hash changed (actual 0x{:016x}, golden 0x{hash:016x})",
        state.state_hash()
    );
    assert_eq!(state.generation(), generation, "{name}: generation changed");
    assert_eq!(state.title(), title, "{name}: title changed");
}

#[test]
fn golden_version_is_pinned() {
    assert_eq!(
        bitty_term_state::canonical_public::CANONICAL_HASH_VERSION,
        HASH_VERSION,
        "M1-09 goldens must be re-recorded when the canonical hash version bumps"
    );
}

/// The 16 base and bright ANSI colors resolve to distinct indexed attributes
/// in row-major order: `SGR 30–37`, `90–97` foregrounds then `40–47`,
/// `100–107` backgrounds.
#[test]
fn golden_sgr_16_color_fg_and_bg_cells() {
    assert_golden("01-sgr-16-fg-bg.bin", 0x6317_39df_70de_777b, 67, "");
    let state = state_of("01-sgr-16-fg-bg.bin");
    let cells = state.snapshot().cells;
    for i in 0..16u8 {
        let glyph = char::from(b'a' + i);
        let cell = cells[usize::from(i)];
        assert_eq!(cell.glyph, glyph, "foreground sweep cell {i}");
        assert_eq!(
            cell.style.foreground,
            Some(Color::Indexed(i)),
            "foreground SGR {} maps to indexed {i}",
            30 + i
        );
        assert_eq!(
            cell.style.background, None,
            "foreground-only cell {i} must not carry a background"
        );
        let bg_glyph = char::from(b'A' + i);
        let bg_cell = cells[16 + usize::from(i)];
        assert_eq!(bg_cell.glyph, bg_glyph, "background sweep cell {i}");
        assert_eq!(
            bg_cell.style.background,
            Some(Color::Indexed(i)),
            "background SGR {} maps to indexed {i}",
            40 + i
        );
        assert_eq!(
            bg_cell.style.foreground, None,
            "the SGR 0 reset before the background sweep clears the foreground"
        );
    }
}

/// 256-color indexed samples across the cube (`16–231`) and grayscale ramp
/// (`232–255`) resolve to the same indexed attribute for fg and bg.
#[test]
fn golden_sgr_256_indexed_cells() {
    assert_golden("02-sgr-256-indexed.bin", 0x70e3_1338_61bc_29b5, 32, "");
    let state = state_of("02-sgr-256-indexed.bin");
    let cells = state.snapshot().cells;
    let samples: [u8; 10] = [16, 21, 52, 88, 196, 201, 231, 232, 240, 255];
    for (col, index) in samples.iter().enumerate() {
        let cell = cells[col];
        assert_eq!(cell.glyph, 'K');
        assert_eq!(cell.style.foreground, Some(Color::Indexed(*index)));
        assert_eq!(cell.style.background, Some(Color::Indexed(*index)));
    }
}

/// Truecolor foreground and background resolve to exact `Color::Rgb` values.
#[test]
fn golden_sgr_truecolor_cells() {
    assert_golden("03-sgr-truecolor.bin", 0x28e3_fd7c_5045_54f6, 10, "");
    let state = state_of("03-sgr-truecolor.bin");
    let cells = state.snapshot().cells;
    assert_eq!(
        cells[0].style.foreground,
        Some(Color::Rgb(Rgb { r: 0, g: 0, b: 0 }))
    );
    assert_eq!(
        cells[1].style.foreground,
        Some(Color::Rgb(Rgb {
            r: 255,
            g: 255,
            b: 255
        }))
    );
    assert_eq!(
        cells[2].style.background,
        Some(Color::Rgb(Rgb {
            r: 18,
            g: 52,
            b: 86
        }))
    );
    assert_eq!(
        cells[3].style.foreground,
        Some(Color::Rgb(Rgb {
            r: 205,
            g: 214,
            b: 244
        }))
    );
    assert_eq!(
        cells[3].style.background,
        Some(Color::Rgb(Rgb {
            r: 18,
            g: 52,
            b: 86
        })),
        "the truecolor background persists until the SGR 0 reset"
    );
}

/// OSC 0 and OSC 2 both set the title; the later OSC 2 wins.
#[test]
fn golden_osc_title_set() {
    assert_golden(
        "04-osc-title.bin",
        0x98a7_108c_66a7_d93f,
        2,
        "bitty-title-2",
    );
}

/// OSC 10/11 queries are inert for grid truth: no cell, mode, or title
/// change, so the canonical hash matches a bare state.
#[test]
fn golden_osc_color_queries_are_grid_inert() {
    assert_golden("05-osc-color-query.bin", 0x8f00_84e8_ce98_ef7e, 2, "");
    assert_golden("06-osc-color-set-query.bin", 0x8f00_84e8_ce98_ef7e, 4, "");
    assert_eq!(
        state_of("05-osc-color-query.bin").state_hash(),
        State::new().state_hash(),
        "a pure OSC 10/11 query must not mutate Terminal Truth"
    );
    assert_eq!(
        state_of("06-osc-color-set-query.bin").state_hash(),
        State::new().state_hash(),
        "OSC 10/11 sets are runtime-owned and grid-inert by contract (CTX-0381)"
    );
}

/// OSC 2 sets a title, SGR styles text, then OSC 0 with an empty payload
/// resets the title to the empty string; the styled cells survive.
#[test]
fn golden_osc_title_reset_leaves_styled_text() {
    assert_golden("07-osc-title-reset.bin", 0x1211_9ae9_3dea_5dfd, 11, "");
    let state = state_of("07-osc-title-reset.bin");
    let cells = state.snapshot().cells;
    let text: String = cells[..6].iter().map(|c| c.glyph).collect();
    assert_eq!(text, "titled");
    for cell in &cells[..6] {
        assert_eq!(cell.style.foreground, Some(Color::Indexed(2)));
    }
}
