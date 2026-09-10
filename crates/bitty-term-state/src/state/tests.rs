//! Unit tests for the terminal truth state machine.

use super::*;
use crate::scrollback::SCROLLBACK_DEFAULT_LINES;
use bitty_vt::{AttributeChange, AttributeDiff, Color, ControlChar, GraphemeCell};

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

#[test]
fn print_places_glyphs_and_advances_cursor() {
    let mut s = State::new();
    prints(&mut s, "ab\u{4E2D}");
    assert_eq!(s.cursor().position.col, 4);
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, 'a');
    assert_eq!(snap.cells[2].glyph, '\u{4E2D}');
    assert_eq!(snap.cells[2].width, 2);
    assert!(snap.cells[3].spacer);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn combining_mark_attaches_to_preceding_cell_without_advance() {
    let mut s = State::new();
    // Decomposed e + acute (CR-TERM-01): the mark must survive on the
    // base cell instead of being silently dropped.
    prints(&mut s, "e\u{0301}");
    assert_eq!(s.cursor().position.col, 1);
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, 'e');
    assert_eq!(snap.cells[0].zerowidth, vec!['\u{0301}']);
    assert!(snap.cells[1].is_blank());
    assert!(s.check_invariants().is_ok());
    // ZWJ attaches the same way (complex-script joiner).
    prints(&mut s, "a\u{200D}");
    let snap = s.snapshot();
    assert_eq!(snap.cells[1].glyph, 'a');
    assert_eq!(snap.cells[1].zerowidth, vec!['\u{200D}']);
    assert_eq!(s.cursor().position.col, 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn combining_buffer_is_bounded_and_drops_excess() {
    use crate::cell::MAX_ZEROWIDTH_CHARS;
    let mut s = State::new();
    prints(&mut s, "x");
    for _ in 0..(MAX_ZEROWIDTH_CHARS + 10) {
        prints(&mut s, "\u{0301}");
    }
    // No unbounded growth: the buffer caps and the cursor never moves.
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, 'x');
    assert_eq!(snap.cells[0].zerowidth.len(), MAX_ZEROWIDTH_CHARS);
    assert!(snap.cells[0].zerowidth.iter().all(|m| *m == '\u{0301}'));
    assert_eq!(s.cursor().position.col, 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn leading_combining_mark_without_base_is_dropped() {
    let mut s = State::new();
    // No preceding cell exists at the top-left corner: the mark is
    // dropped and the grid stays blank.
    prints(&mut s, "\u{0301}");
    let snap = s.snapshot();
    assert!(snap.cells[0].is_blank());
    assert!(snap.cells[0].zerowidth.is_empty());
    assert_eq!(s.cursor().position.col, 0);
    assert_eq!(s.cursor().position.row, 0);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn combining_after_wide_char_attaches_to_lead_half() {
    let mut s = State::new();
    prints(&mut s, "\u{4E2D}\u{0301}");
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{4E2D}');
    assert_eq!(snap.cells[0].zerowidth, vec!['\u{0301}']);
    assert!(snap.cells[1].spacer);
    assert!(snap.cells[1].zerowidth.is_empty());
    assert_eq!(s.cursor().position.col, 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn combining_preserves_deferred_wrap_latch() {
    let mut s = State::new();
    // Fill the first row so the cursor latches a deferred wrap.
    for _ in 0..GRID_COLUMNS {
        prints(&mut s, "x");
    }
    assert!(s.cursor().pending_wrap);
    // A combining mark attaches to the last cell and must NOT consume
    // the latch: the next full-width print still wraps.
    prints(&mut s, "\u{0301}");
    assert!(s.cursor().pending_wrap);
    assert_eq!(s.cursor().position.row, 0);
    let snap = s.snapshot();
    assert_eq!(snap.cells[GRID_COLUMNS - 1].zerowidth, vec!['\u{0301}']);
    prints(&mut s, "y");
    assert_eq!(s.cursor().position.row, 1);
    assert_eq!(s.cursor().position.col, 1);
    assert!(!s.cursor().pending_wrap);
    let snap = s.snapshot();
    assert_eq!(snap.cells[GRID_COLUMNS].glyph, 'y');
    assert!(s.check_invariants().is_ok());
}

#[test]
fn combining_mark_changes_state_hash() {
    let mut plain = State::new();
    prints(&mut plain, "e");
    let mut marked = State::new();
    prints(&mut marked, "e\u{0301}");
    // Combining content is Terminal Truth: it must enter the hash.
    assert_ne!(plain.state_hash(), marked.state_hash());
    let mut marked2 = State::new();
    prints(&mut marked2, "e\u{0301}");
    assert_eq!(marked.state_hash(), marked2.state_hash());
}

#[test]
fn deferred_wrap_latches_at_last_column() {
    let mut s = State::new();
    s.cursor.position.col = GRID_COLUMNS as u16 - 1;
    prints(&mut s, "x");
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 1);
    assert!(s.cursor().pending_wrap);
    // Next print consumes the latch onto the next line.
    prints(&mut s, "y");
    assert_eq!(s.cursor().position.col, 1);
    assert_eq!(s.cursor().position.row, 1);
    assert!(!s.cursor().pending_wrap);
}

#[test]
fn origin_mode_addresses_relative_to_region() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(5),
        bottom: Row(10),
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    // DECOM homes into the region.
    assert_eq!(s.cursor().position.row, 4);
    s.apply(&TerminalAction::CursorPosition {
        row: Row(1),
        col: Col(3),
    });
    assert_eq!(s.cursor().position.row, 4);
    assert_eq!(s.cursor().position.col, 2);
}

#[test]
fn invalid_scroll_region_is_ignored() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(10),
        bottom: Row(5),
    });
    assert_eq!(s.scroll_region_top, 0);
    assert_eq!(s.scroll_region_bottom, GRID_ROWS as u16 - 1);
}

#[test]
fn alt_screen_roundtrip_restores_primary_set() {
    let mut s = State::new();
    prints(&mut s, "primary");
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![
                AttributeChange::Enable(bitty_vt::Attribute::Bold),
                AttributeChange::Foreground(Color::Indexed(1)),
            ]
            .into_boxed_slice(),
        },
    });
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Down,
        n: Count(3),
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::BracketedPaste,
        enabled: true,
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    // Mutate the alternate context aggressively: modes flipped on alt
    // must NOT leak into the restored primary set (invariant 5), while
    // the pre-entry bracketed-paste state must come back.
    prints(&mut s, "alt junk");
    s.apply(&TerminalAction::SetMode {
        mode: Mode::BracketedPaste,
        enabled: false,
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    s.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::All,
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    // Full primary-screen cursor/style/mode set restored (invariant 5).
    assert!(!s.alt_screen_active());
    assert!(
        s.modes.bracketed_paste,
        "pre-entry mode state must survive the roundtrip"
    );
    assert!(!s.modes.origin);
    assert!(s.cursor().style.attributes.bold);
    assert_eq!(
        s.cursor().style.foreground,
        Some(Color::Indexed(1)),
        "pen style must survive the roundtrip"
    );
    assert_eq!(s.cursor().position.col, 7);
    let snap = s.snapshot();
    assert_eq!(
        &snap.cells[..7].iter().map(|c| c.glyph).collect::<String>(),
        "primary"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn scroll_under_screen_bottom_captures_scrollback() {
    let mut s = State::new();
    prints(&mut s, "line one");
    for _ in 0..(GRID_ROWS + 3) {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    assert!(
        s.scrollback_len() > 0 && s.scrollback_len() <= SCROLLBACK_DEFAULT_LINES,
        "indexing at the screen bottom must feed scrollback"
    );
    assert_eq!(
        s.scrollback_line(0).unwrap().cells[0].glyph,
        'l',
        "oldest captured line first"
    );
    // Partial regions never capture (invariant 4).
    let before = s.scrollback_len();
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(2),
        bottom: Row(10),
    });
    s.apply(&TerminalAction::ScrollUp { n: Count(3) });
    assert_eq!(s.scrollback_len(), before);
}

#[test]
fn configured_scrollback_capacity_bounds_retention() {
    const CAP: usize = 3;
    let mut s = State::with_scrollback_lines(CAP);
    prints(&mut s, "line one");
    for _ in 0..(GRID_ROWS + 10) {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    assert_eq!(
        s.scrollback_len(),
        CAP,
        "retention must stop at the configured capacity"
    );
    assert!(s.check_invariants().is_ok());
    // Oldest-first pruning: the retained tail is the newest content.
    assert_eq!(s.scrollback().count(), CAP);
}

#[test]
fn zero_scrollback_capacity_retains_nothing() {
    let mut s = State::with_scrollback_lines(0);
    prints(&mut s, "line one");
    for _ in 0..(GRID_ROWS + 10) {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    assert_eq!(s.scrollback_len(), 0);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn reply_synthesis_is_origin_aware_and_bounded() {
    let mut s = State::new();
    s.apply(&TerminalAction::RequestDeviceStatus {
        kind: StatusKind::OperatingStatus,
    });
    s.apply(&TerminalAction::RequestDeviceStatus {
        kind: StatusKind::DeviceAttributes,
    });
    let replies = s.take_replies();
    assert_eq!(replies.len(), 2);
    assert_eq!(&replies[0][..], b"\x1b[0n");
    assert_eq!(&replies[1][..], b"\x1b[?6c");
    // CPR reflects origin-relative rows.
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(5),
        bottom: Row::SENTINEL,
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    s.apply(&TerminalAction::RequestDeviceStatus {
        kind: StatusKind::CursorPosition,
    });
    let replies = s.take_replies();
    assert_eq!(&replies[0][..], b"\x1b[1;1R");
}

#[test]
fn decstr_resets_defined_subset_only() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Enable(bitty_vt::Attribute::Bold)].into_boxed_slice(),
        },
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    s.apply(&TerminalAction::SoftReset);
    assert!(s.cursor().visible);
    assert!(!s.modes.origin);
    assert!(!s.modes.auto_wrap, "DECSTR resets DECAWM per VT510");
    assert_eq!(s.cursor().style, Style::default());
    assert_eq!(s.scroll_region_bottom, GRID_ROWS as u16 - 1);
}

#[test]
fn full_reset_restores_initial_truth() {
    let mut s = State::new();
    prints(&mut s, "junk \u{4E2D} more");
    s.apply(&TerminalAction::OscTitle {
        text: BoundedString::new("t"),
    });
    s.apply(&TerminalAction::FullReset);
    assert!(s.check_invariants().is_ok());
    assert_eq!(s.state_hash(), State::new().state_hash());
    assert!(s.title().is_empty());
    assert_eq!(s.scrollback_len(), 0);
    let snap = s.snapshot();
    assert!(snap.cells.iter().all(Cell::is_blank));
}

#[test]
fn sgr_reset_clears_pen_colors_for_bce_blanks() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Background(Color::Rgb(bitty_vt::Rgb {
                r: 231,
                g: 236,
                b: 248,
            }))]
            .into_boxed_slice(),
        },
    });
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Reset].into_boxed_slice(),
        },
    });
    assert_eq!(s.cursor().style, Style::default());
    for _ in 0..(GRID_ROWS + 3) {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    let snap = s.snapshot();
    assert!(
        snap.cells.iter().all(|c| c.style.background.is_none()),
        "BCE blanks after SGR 0 must use the default background"
    );
}

#[test]
fn decscusr_tracks_style_in_snapshot_and_ris_resets() {
    // CTX-0162: `CSI Ps SP q` (0/1 block, 2 steady block, 3/4 underline,
    // 5/6 bar) lands in the snapshot cursor; RIS restores the default.
    let mut s = State::new();
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::Default);
    for style in [
        CursorStyle::BlinkingBlock,
        CursorStyle::SteadyBlock,
        CursorStyle::BlinkingUnderline,
        CursorStyle::SteadyUnderline,
        CursorStyle::BlinkingBar,
        CursorStyle::SteadyBar,
    ] {
        s.apply(&TerminalAction::CursorStyle { style });
        assert_eq!(s.cursor().cursor_style, style);
        assert_eq!(s.snapshot().cursor.cursor_style, style);
        assert!(s.check_invariants().is_ok());
    }
    s.apply(&TerminalAction::FullReset);
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::Default);
    assert_eq!(s.state_hash(), State::new().state_hash());
}

#[test]
fn alt_screen_exit_restores_saved_cursor_style() {
    // CTX-0162: alt-screen apps (nvim) set their own DECSCUSR shape;
    // leaving must restore the primary shape instead of leaking bar.
    let mut s = State::new();
    s.apply(&TerminalAction::CursorStyle {
        style: CursorStyle::SteadyBlock,
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    s.apply(&TerminalAction::CursorStyle {
        style: CursorStyle::SteadyBar,
    });
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::SteadyBar);
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert_eq!(
        s.snapshot().cursor.cursor_style,
        CursorStyle::SteadyBlock,
        "primary DECSCUSR shape must survive the alt-screen roundtrip"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0182_large_altscreen_fullscreen_stays_blue() {
    // nmtui live pattern at 200x62 (CTX-0182, issue #282): alt-screen
    // entry, ED All with black BCE, then blue EL fills for every row.
    // The top row must stay blue (Indexed 4), never dark stale: the
    // observed top-dark/bottom-blue split was a GPU multi-chunk submit
    // bug, not state, so this locks the state half of the contract.
    let mut s = State::new();
    s.resize(200, 62);
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    // ED All with black bg (nmtui sets 37m/40m before H/2J).
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Background(Color::Indexed(0))].into_boxed_slice(),
        },
    });
    s.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::All,
    });
    // Blue EL fill per row (97m/44m + EL Right + LF).
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Background(Color::Indexed(4))].into_boxed_slice(),
        },
    });
    for _ in 0..62 {
        s.apply(&TerminalAction::EraseInLine {
            mode: EraseLineMode::Right,
        });
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    let snap = s.snapshot();
    assert_eq!((snap.width, snap.height), (200, 62));
    for (i, cell) in snap.cells.iter().enumerate().take(snap.width) {
        assert_eq!(
            cell.style.background,
            Some(Color::Indexed(4)),
            "top row col {i} must be nmtui blue"
        );
    }
    assert!(
        snap.cells
            .iter()
            .all(|c| c.style.background == Some(Color::Indexed(4))),
        "every fullscreen row must be blue after EL fills"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_large_cursor_right_stops_at_margin() {
    // CR-TERM-02: a 65535-count CUF must terminate at the right margin
    // instead of burning 65k grid probes.
    let mut s = State::new();
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Right,
        n: Count(u16::MAX),
    });
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_small_cursor_moves_stay_exact() {
    // Normal moves keep exact step semantics after the margin breaks.
    let mut s = State::new();
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Right,
        n: Count(3),
    });
    assert_eq!(s.cursor().position.col, 3);
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Left,
        n: Count(2),
    });
    assert_eq!(s.cursor().position.col, 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_large_cursor_left_stops_at_zero() {
    // CR-TERM-02: symmetric CUB bound at the left margin.
    let mut s = State::new();
    s.apply(&TerminalAction::CursorPosition {
        row: Row::SENTINEL,
        col: Col(71),
    });
    assert_eq!(s.cursor().position.col, 70);
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Left,
        n: Count(u16::MAX),
    });
    assert_eq!(s.cursor().position.col, 0);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_wide_pair_at_margin_right_is_stable() {
    // A wide pair ending at the last column is the Right fixed point:
    // stepping right returns to the leading half and must terminate.
    let mut s = State::new();
    prints(&mut s, &"a".repeat(GRID_COLUMNS - 2));
    prints(&mut s, "中");
    s.apply(&TerminalAction::CursorPosition {
        row: Row::SENTINEL,
        col: Col(GRID_COLUMNS as u16 - 1),
    });
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 2);
    s.apply(&TerminalAction::CursorMove {
        dir: Direction::Right,
        n: Count(u16::MAX),
    });
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_large_tab_forward_stops_at_margin() {
    // CR-TERM-02: a 65535-count tab run must terminate at the right
    // margin instead of burning 65k lattice scans.
    let mut s = State::new();
    s.apply(&TerminalAction::TabForward { n: Count(u16::MAX) });
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_small_tab_steps_stay_exact() {
    // Normal tab steps keep exact lattice semantics after the breaks.
    let mut s = State::new();
    s.apply(&TerminalAction::TabForward { n: Count(1) });
    assert_eq!(s.cursor().position.col, 8);
    s.apply(&TerminalAction::TabBackward { n: Count(1) });
    assert_eq!(s.cursor().position.col, 0);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0205_large_tab_backward_stops_at_zero() {
    // CR-TERM-02: symmetric backward-tab bound at column 0.
    let mut s = State::new();
    s.apply(&TerminalAction::CursorPosition {
        row: Row::SENTINEL,
        col: Col(GRID_COLUMNS as u16),
    });
    assert_eq!(s.cursor().position.col, GRID_COLUMNS as u16 - 1);
    s.apply(&TerminalAction::TabBackward { n: Count(u16::MAX) });
    assert_eq!(s.cursor().position.col, 0);
    assert!(s.check_invariants().is_ok());
}
