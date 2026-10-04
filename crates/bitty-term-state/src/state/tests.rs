//! Unit tests for the terminal truth state machine.

use super::*;
use crate::scrollback::SCROLLBACK_DEFAULT_LINES;
use bitty_vt::{
    AttributeChange, AttributeDiff, CharsetSlot, CharsetTable, Color, ControlChar, GraphemeCell,
};

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
    assert_eq!(snap.cells[0].zerowidth.as_slice(), &['\u{0301}']);
    assert!(snap.cells[1].is_blank());
    assert!(s.check_invariants().is_ok());
    // ZWJ attaches the same way (complex-script joiner).
    prints(&mut s, "a\u{200D}");
    let snap = s.snapshot();
    assert_eq!(snap.cells[1].glyph, 'a');
    assert_eq!(snap.cells[1].zerowidth.as_slice(), &['\u{200D}']);
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
    assert_eq!(snap.cells[0].zerowidth.as_slice(), &['\u{0301}']);
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
    assert_eq!(
        snap.cells[GRID_COLUMNS - 1].zerowidth.as_slice(),
        &['\u{0301}']
    );
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
fn enhanced_keyboard_stack_set_push_pop_and_query_reply() {
    use bitty_vt::{EnhancedKeyboardOp, EnhancedKeyboardSetMode};
    let mut s = State::new();
    let op = |op| TerminalAction::EnhancedKeyboard { op };
    s.apply(&op(EnhancedKeyboardOp::Set {
        flags: 3,
        mode: EnhancedKeyboardSetMode::Assign,
    }));
    assert_eq!(s.modes.enhanced_keyboard.flags(), 3);
    s.apply(&op(EnhancedKeyboardOp::Push { flags: 8 }));
    assert_eq!(s.modes.enhanced_keyboard.flags(), 8);
    assert_eq!(s.modes.enhanced_keyboard.depth(), 2);
    s.apply(&op(EnhancedKeyboardOp::Pop { n: 1 }));
    assert_eq!(s.modes.enhanced_keyboard.flags(), 3);
    // Bounded: an oversized pop empties the stack and resets the flags.
    s.apply(&op(EnhancedKeyboardOp::Pop { n: u16::MAX }));
    assert_eq!(s.modes.enhanced_keyboard.flags(), 0);
    assert_eq!(s.modes.enhanced_keyboard.depth(), 0);
    // Query replies with the live flags without touching state.
    s.apply(&op(EnhancedKeyboardOp::Set {
        flags: 31,
        mode: EnhancedKeyboardSetMode::Assign,
    }));
    s.apply(&op(EnhancedKeyboardOp::Query));
    let replies = s.take_replies();
    assert_eq!(&replies[0][..], b"\x1b[?31u");
}

#[test]
fn enhanced_keyboard_stack_is_bounded_and_evicts_oldest() {
    let mut s = State::new();
    for flags in 0..12u32 {
        s.apply(&TerminalAction::EnhancedKeyboard {
            op: bitty_vt::EnhancedKeyboardOp::Push { flags },
        });
    }
    assert_eq!(
        s.modes.enhanced_keyboard.depth(),
        crate::modes::ENHANCED_KEYBOARD_STACK_MAX
    );
    // Oldest entries were evicted: the top is the last pushed value.
    assert_eq!(s.modes.enhanced_keyboard.flags(), 11);
}

#[test]
fn enhanced_keyboard_stack_participates_in_the_state_hash() {
    let mut pushed = State::new();
    pushed.apply(&TerminalAction::EnhancedKeyboard {
        op: bitty_vt::EnhancedKeyboardOp::Set {
            flags: 1,
            mode: bitty_vt::EnhancedKeyboardSetMode::Assign,
        },
    });
    pushed.apply(&TerminalAction::EnhancedKeyboard {
        op: bitty_vt::EnhancedKeyboardOp::Push { flags: 1 },
    });
    // Same live flags, different stack depth: hashes must differ (v8).
    let mut flat = State::new();
    flat.apply(&TerminalAction::EnhancedKeyboard {
        op: bitty_vt::EnhancedKeyboardOp::Set {
            flags: 1,
            mode: bitty_vt::EnhancedKeyboardSetMode::Assign,
        },
    });
    assert_eq!(
        pushed.modes.enhanced_keyboard.flags(),
        flat.modes.enhanced_keyboard.flags()
    );
    assert_ne!(pushed.state_hash(), flat.state_hash());
}

#[test]
fn enhanced_keyboard_register_is_per_screen() {
    // F3 (review PX-3072): entering the alternate screen switches to a fresh
    // independent register; main's register is restored on exit and the alt
    // register survives re-entry.
    let mut s = State::new();
    s.apply(&TerminalAction::EnhancedKeyboard {
        op: bitty_vt::EnhancedKeyboardOp::Set {
            flags: 4,
            mode: bitty_vt::EnhancedKeyboardSetMode::Assign,
        },
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert_eq!(
        s.modes.enhanced_keyboard.flags(),
        0,
        "already-negotiated main flags must not leak into a fresh alt screen"
    );
    s.apply(&TerminalAction::EnhancedKeyboard {
        op: bitty_vt::EnhancedKeyboardOp::Set {
            flags: 2,
            mode: bitty_vt::EnhancedKeyboardSetMode::Assign,
        },
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert_eq!(
        s.modes.enhanced_keyboard.flags(),
        4,
        "main register restored"
    );
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert_eq!(
        s.modes.enhanced_keyboard.flags(),
        2,
        "alt register survives a re-entry"
    );
}

#[test]
fn enhanced_keyboard_stash_participates_in_the_state_hash() {
    // Two states with identical live flags but different inactive-screen
    // registers are not behaviorally identical (v9).
    let negotiate_and_leave = |alt_flags: Option<u32>| {
        let mut s = State::new();
        if let Some(flags) = alt_flags {
            s.apply(&TerminalAction::SetMode {
                mode: Mode::AlternateScreenClearAndRestore,
                enabled: true,
            });
            s.apply(&TerminalAction::EnhancedKeyboard {
                op: bitty_vt::EnhancedKeyboardOp::Set {
                    flags,
                    mode: bitty_vt::EnhancedKeyboardSetMode::Assign,
                },
            });
            s.apply(&TerminalAction::SetMode {
                mode: Mode::AlternateScreenClearAndRestore,
                enabled: false,
            });
        }
        s
    };
    let with_stash = negotiate_and_leave(Some(2));
    let without_stash = negotiate_and_leave(None);
    assert_eq!(with_stash.modes.enhanced_keyboard.flags(), 0);
    assert_eq!(without_stash.modes.enhanced_keyboard.flags(), 0);
    assert_ne!(with_stash.state_hash(), without_stash.state_hash());
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
fn synchronized_update_mode_is_tracked_and_nested_begins_end_once() {
    let mut s = State::new();
    assert!(!s.modes().synchronized_update, "off at power-on");
    // Nested begins are idempotent mode sets; one reset ends the batch.
    for enabled in [true, true, false] {
        s.apply(&TerminalAction::SetMode {
            mode: Mode::SynchronizedUpdate,
            enabled,
        });
    }
    assert!(!s.modes().synchronized_update);
    s.apply(&TerminalAction::SetMode {
        mode: Mode::SynchronizedUpdate,
        enabled: true,
    });
    assert!(s.modes().synchronized_update);
}

#[test]
fn alternate_scroll_mode_is_tracked_and_hashed() {
    // CTX-0566 (#970): mode 1007 is stored in the mode register and enters
    // the canonical state hash, so replay determinism covers it.
    let mut s = State::new();
    assert!(!s.modes().alternate_scroll, "off at power-on");
    let base = s.state_hash();
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScroll,
        enabled: true,
    });
    assert!(s.modes().alternate_scroll);
    assert_ne!(
        s.state_hash(),
        base,
        "alternate scroll must be truth-bearing (hashed)"
    );
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScroll,
        enabled: false,
    });
    assert!(!s.modes().alternate_scroll);
    assert_eq!(s.state_hash(), base, "reset restores the pre-set hash");
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
fn configured_default_cursor_style_seeds_and_resolves_resets() {
    // CTX-0756 (issue #1359 `terminal.cursor_style`): one setter call at
    // creation seeds a fresh pane and resolves every later app `DECSCUSR 0`
    // reset back to the configured shape; explicit app shapes still apply;
    // RIS restores the configured shape (not the hardcoded fallback); and
    // the setter never clobbers an app-reshaped live cursor.
    let mut s = State::new();
    s.set_default_cursor_style(CursorStyle::SteadyBar);
    assert_eq!(s.default_cursor_style(), CursorStyle::SteadyBar);
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::SteadyBar);
    // App reset (CSI 0 SP q) resolves back to the configured shape.
    s.apply(&TerminalAction::CursorStyle {
        style: CursorStyle::Default,
    });
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::SteadyBar);
    // Explicit app shapes still win at runtime.
    s.apply(&TerminalAction::CursorStyle {
        style: CursorStyle::BlinkingUnderline,
    });
    assert_eq!(
        s.snapshot().cursor.cursor_style,
        CursorStyle::BlinkingUnderline
    );
    // Re-seeding while the app owns the shape only moves the stored
    // default, never the live cursor.
    s.set_default_cursor_style(CursorStyle::SteadyBlock);
    assert_eq!(s.default_cursor_style(), CursorStyle::SteadyBlock);
    assert_eq!(
        s.snapshot().cursor.cursor_style,
        CursorStyle::BlinkingUnderline
    );
    // RIS restores the configured shape.
    s.apply(&TerminalAction::FullReset);
    assert_eq!(s.snapshot().cursor.cursor_style, CursorStyle::SteadyBlock);
    assert!(s.check_invariants().is_ok());
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
fn alt_screen_47_does_not_save_or_restore_the_cursor() {
    // Issue #1173 / CTX-0582. Mode ?47 is the legacy alternate-buffer switch:
    // xterm `srm_ALTBUF` performs no CursorSave/CursorRestore and ghostty
    // `.@"47"` "only copies the cursor", so a ?47 round trip leaves the cursor
    // wherever the alt screen left it. Bitty previously snapshotted the cursor
    // for Via47 as well, so "X ?47h Y ?47l Z" restored the cursor onto X and
    // produced "XZ" instead of the reference "X Z".
    let mut s = State::new();
    prints(&mut s, "X");
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreen,
        enabled: true,
    });
    assert_eq!(
        s.cursor().position.col,
        1,
        "?47 entry must leave the cursor in place"
    );
    prints(&mut s, "Y");
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreen,
        enabled: false,
    });
    assert_eq!(
        s.cursor().position.col,
        2,
        "?47 exit must leave the alt cursor in place"
    );
    prints(&mut s, "Z");
    let snap = s.snapshot();
    assert_eq!(
        &snap.cells[..3].iter().map(|c| c.glyph).collect::<String>(),
        "X Z",
        "reference xterm/ghostty place Z after X on the primary row"
    );
    assert_eq!(s.cursor().position.col, 3);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn alt_screen_1049_still_saves_and_restores_the_cursor() {
    // The ?1049 counterpart of the test above: xterm
    // `srm_OPT_ALTBUF_CURSOR` saves the cursor as in DECSC on entry and
    // restores it on exit, so the same byte stream returns to the pre-entry
    // column and "X ?1049h Y ?1049l Z" yields "XZ" (issue #1173 acceptance:
    // ?1049 behavior is unchanged).
    let mut s = State::new();
    prints(&mut s, "X");
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    prints(&mut s, "Y");
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert_eq!(
        s.cursor().position.col,
        1,
        "?1049 exit must restore the pre-entry cursor"
    );
    prints(&mut s, "Z");
    let snap = s.snapshot();
    assert_eq!(
        &snap.cells[..2].iter().map(|c| c.glyph).collect::<String>(),
        "XZ"
    );
    assert_eq!(s.cursor().position.col, 2);
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

#[test]
fn locking_shift_g2_locks_gl_until_changed() {
    // LS2 (`ESC n`) submits `InvokeCharset G2`: GL stays on G2 for every
    // subsequent print until another locking shift changes it.
    let mut s = State::new();
    s.apply(&TerminalAction::SelectCharset {
        slot: CharsetSlot::G2,
        table: CharsetTable::DecSpecialGraphics,
    });
    s.apply(&TerminalAction::InvokeCharset {
        slot: CharsetSlot::G2,
    });
    prints(&mut s, "qq");
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{2500}');
    assert_eq!(snap.cells[1].glyph, '\u{2500}');
    // SI locks GL back to G0 (ASCII).
    s.apply(&TerminalAction::InvokeCharset {
        slot: CharsetSlot::G0,
    });
    prints(&mut s, "q");
    assert_eq!(s.snapshot().cells[2].glyph, 'q');
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0469_origin_cpr_with_cursor_above_region_saturates() {
    // Hostile probe (CTX-0469): DECSC saves (row, origin) together, but a
    // later DECSTBM homes only the live cursor. DECRC then restores a
    // cursor above the region with origin mode on; the post-action clamp
    // pulls it into the region and DSR-CPR stays sane (no panic).
    let mut s = State::new();
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(5),
        bottom: Row(10),
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    assert_eq!(s.cursor().position.row, 4);
    s.apply(&TerminalAction::CursorSave);
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(8),
        bottom: Row(10),
    });
    s.apply(&TerminalAction::CursorRestore);
    // Restored above the region: clamped to the region top.
    assert_eq!(s.cursor().position.row, 7);
    assert!(s.modes().origin);
    s.apply(&TerminalAction::RequestDeviceStatus {
        kind: StatusKind::CursorPosition,
    });
    let replies = s.take_replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(&replies[0][..], b"\x1b[1;1R");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn single_shift_g2_applies_to_one_scalar_only() {
    // SS2 (`ESC N`) submits `SingleShiftCharset G2`: exactly one print is
    // translated and the locking shift is untouched.
    let mut s = State::new();
    s.apply(&TerminalAction::SelectCharset {
        slot: CharsetSlot::G2,
        table: CharsetTable::DecSpecialGraphics,
    });
    s.apply(&TerminalAction::SingleShiftCharset {
        slot: CharsetSlot::G2,
    });
    prints(&mut s, "qq");
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{2500}');
    assert_eq!(snap.cells[1].glyph, 'q');
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0469_origin_cpr_bare_subtraction_saturates() {
    // White-box companion (CTX-0469): even if the cursor ever sits above
    // the region with origin mode on, the CPR synthesis must saturate
    // instead of panicking on a bare u16 subtraction.
    let mut s = State::new();
    s.cursor.position.row = 4;
    s.cursor.position.col = 2;
    s.modes.origin = true;
    s.scroll_region_top = 7;
    s.scroll_region_bottom = 9;
    s.request_device_status(StatusKind::CursorPosition);
    let replies = s.take_replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(&replies[0][..], b"\x1b[1;3R");
}

#[test]
fn ctx_0469_hyperlink_table_evicts_oldest_and_keeps_accepting() {
    // Hostile probe (CTX-0469): past 1024 distinct links the table must
    // evict oldest-first (ImageStore precedent) instead of degrading every
    // future link to None permanently. Evicted ids fail closed (None).
    use crate::state::HYPERLINK_TABLE_MAX;
    let mut s = State::new();
    let link = |i: usize| bitty_vt::Hyperlink {
        id: None,
        uri: BoundedString::new(format!("https://example.invalid/{i}")),
    };
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(link(0)),
    });
    let first_id = s.current_hyperlink().expect("first link accepted");
    assert_eq!(
        s.hyperlink_entry(first_id),
        Some((None, "https://example.invalid/0"))
    );
    for i in 1..=HYPERLINK_TABLE_MAX + 4 {
        s.apply(&TerminalAction::OscHyperlink {
            link: Some(link(i)),
        });
        assert!(
            s.current_hyperlink().is_some(),
            "link {i} must still be accepted past capacity"
        );
    }
    assert_eq!(s.hyperlink_count(), HYPERLINK_TABLE_MAX);
    assert!(
        s.hyperlink_entry(first_id).is_none(),
        "evicted id must fail closed"
    );
    let newest = s.current_hyperlink().expect("newest link accepted");
    assert_eq!(
        s.hyperlink_entry(newest),
        Some((None, "https://example.invalid/1028"))
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0490_hyperlink_lookup_resolves_contiguous_window_after_eviction() {
    // CTX-0490 (item 1): FIFO issuance keeps the resident window contiguous
    // and ascending. Lookup must resolve every resident id and fail closed
    // outside the window; the O(1) front-id arithmetic relies on this
    // invariant, so pin it.
    use crate::state::HYPERLINK_TABLE_MAX;
    let mut s = State::new();
    let link = |i: usize| bitty_vt::Hyperlink {
        id: None,
        uri: BoundedString::new(format!("https://example.invalid/{i}")),
    };
    for i in 0..HYPERLINK_TABLE_MAX + 7 {
        s.apply(&TerminalAction::OscHyperlink {
            link: Some(link(i)),
        });
    }
    assert_eq!(s.hyperlink_count(), HYPERLINK_TABLE_MAX);
    let window: Vec<(HyperlinkId, Option<&str>, &str)> = s.hyperlink_table().collect();
    let first = window[0].0.as_u32();
    for (offset, (id, id_param, uri)) in window.iter().enumerate() {
        assert_eq!(
            id.as_u32(),
            first + offset as u32,
            "resident ids must stay contiguous"
        );
        assert_eq!(
            s.hyperlink_entry(*id),
            Some((*id_param, *uri)),
            "resident id {id:?} must resolve to its own uri"
        );
    }
    assert_eq!(window[0].2, "https://example.invalid/7");
    assert_eq!(
        s.hyperlink_entry(HyperlinkId::new(first - 1)),
        None,
        "evicted id fails closed"
    );
    let newest = window[window.len() - 1].0;
    assert_eq!(
        s.hyperlink_entry(HyperlinkId::new(newest.as_u32() + 1)),
        None,
        "unissued id fails closed"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0490_hyperlink_wrap_restarts_id_space_after_clear() {
    // CTX-0490 (item 2): the id space is u32. When the counter would wrap,
    // the table is cleared and issuance restarts at zero — cells that kept a
    // pre-wrap id can then resolve to a post-wrap entry. Pin the documented
    // bound instead of claiming reuse is impossible.
    let mut s = State::new();
    let link = |uri: &str| bitty_vt::Hyperlink {
        id: None,
        uri: BoundedString::new(uri),
    };
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(link("https://pre-wrap.invalid/0")),
    });
    let stale = s.hyperlink_table().next().expect("first entry").0;
    assert_eq!(stale.as_u32(), 0);
    // White-box: park the counter at the wrap boundary instead of issuing
    // 2^32 distinct links.
    s.next_hyperlink_id = u32::MAX;
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(link("https://post-wrap.invalid/")),
    });
    assert_eq!(s.hyperlink_count(), 1, "wrap clears the resident table");
    assert_eq!(s.current_hyperlink().expect("post-wrap link").as_u32(), 0);
    assert_eq!(
        s.hyperlink_entry(stale),
        Some((None, "https://post-wrap.invalid/")),
        "documented bounded reuse: a pre-wrap id can resolve post-wrap"
    );
}

#[test]
fn ctx_0490_state_hash_covers_hyperlink_id_space() {
    // CTX-0490 (item 4): the canonical hash must distinguish states whose
    // only difference is the next hyperlink id to be issued.
    let mut a = State::new();
    let mut b = State::new();
    for i in 0..3 {
        let link = |uri: &str| bitty_vt::Hyperlink {
            id: None,
            uri: BoundedString::new(uri),
        };
        let uri = format!("https://example.invalid/{i}");
        a.apply(&TerminalAction::OscHyperlink {
            link: Some(link(&uri)),
        });
        b.apply(&TerminalAction::OscHyperlink {
            link: Some(link(&uri)),
        });
    }
    assert_eq!(
        a.state_hash(),
        b.state_hash(),
        "identical states hash equal"
    );
    // Same resident table, different id-space position: must not collide.
    b.next_hyperlink_id = 99;
    assert_ne!(a.state_hash(), b.state_hash());
}

#[test]
fn ctx_0469_live_grid_row_matches_snapshot_row() {
    // The zero-copy search/render path must observe exactly what the
    // cloning snapshot path observes.
    let mut s = State::new();
    prints(&mut s, "hello 中 world");
    let snap = s.snapshot();
    assert_eq!(snap.width, GRID_COLUMNS);
    for row in 0..snap.height {
        let live = s.live_grid_row(row).expect("row in bounds");
        let start = row * snap.width;
        assert_eq!(live, &snap.cells[start..start + snap.width]);
    }
    assert!(s.live_grid_row(snap.height).is_none());
}

#[test]
fn ctx_0469_height_only_resize_keeps_wraps_and_content() {
    // End-to-end companion of the grid probe: State height-only resizes
    // preserve continuation flags and overlapping cells.
    let mut s = State::new();
    prints(&mut s, &"x".repeat(GRID_COLUMNS));
    prints(&mut s, "yz");
    let before = s.snapshot();
    assert!(before.cells[GRID_COLUMNS] != before.cells[GRID_COLUMNS + 1]);
    assert!(
        s.screens.main.wrapped(0),
        "row 0 continues onto row 1 after wrap"
    );
    s.resize(GRID_COLUMNS, GRID_ROWS + 4);
    assert_eq!(s.height(), GRID_ROWS + 4);
    assert!(
        s.screens.main.wrapped(0),
        "height-only resize keeps continuation flags"
    );
    let after = s.snapshot();
    assert_eq!(&after.cells[..GRID_COLUMNS], &before.cells[..GRID_COLUMNS]);
    assert!(s.check_invariants().is_ok());
}

/// TERM-ENG-005 / CTX-0555: resize resets scroll region to full screen,
/// preserves origin mode, and restores saved cursor consistently.
#[test]
fn term_eng_005_resize_resets_scroll_region_but_preserves_origin_and_saved_cursor() {
    let mut s = State::new();
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(5),
        bottom: Row(15),
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    assert!(s.modes.origin);
    // Move inside region
    s.apply(&TerminalAction::CursorPosition {
        row: Row(2),
        col: Col(4),
    });
    let pos_before = s.cursor().position;
    // Save cursor with origin mode
    s.apply(&TerminalAction::CursorSave);

    // Resize terminal
    let new_cols = GRID_COLUMNS + 10;
    let new_rows = GRID_ROWS + 5;
    s.resize(new_cols, new_rows);

    // Scroll region reset to full screen
    assert_eq!(s.scroll_region_top, 0);
    assert_eq!(s.scroll_region_bottom, (new_rows - 1) as u16);
    // Origin mode remains enabled
    assert!(s.modes.origin, "resize must not reset origin mode");

    // Restore cursor
    s.apply(&TerminalAction::CursorRestore);
    assert_eq!(s.cursor().position, pos_before);
    assert!(s.modes.origin, "restore cursor preserves origin mode");
    assert!(s.check_invariants().is_ok());
}

/// TERM-ENG-005 / CTX-0555: narrowed scroll region on alternate screen with resize.
#[test]
fn term_eng_005_alternate_screen_narrowed_region_and_resize() {
    let mut s = State::new();
    prints(&mut s, "primary-content");

    // Enter alternate screen
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());

    // Narrow scroll region on alternate screen
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(2),
        bottom: Row(8),
    });
    prints(&mut s, "alt-content");

    // Resize
    let new_cols = GRID_COLUMNS + 5;
    let new_rows = GRID_ROWS + 2;
    s.resize(new_cols, new_rows);

    assert_eq!(s.width(), new_cols);
    assert_eq!(s.height(), new_rows);
    assert_eq!(s.scroll_region_top, 0);
    assert_eq!(s.scroll_region_bottom, (new_rows - 1) as u16);
    assert!(s.alt_screen_active());

    // Leave alternate screen
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert!(!s.alt_screen_active());
    assert_eq!(s.width(), new_cols);
    assert_eq!(s.height(), new_rows);
    assert!(s.check_invariants().is_ok());
}

/// ENG-005c / CTX-0555: alternate screen × hyperlink isolation test gap.
#[test]
fn term_eng_005c_alternate_screen_hyperlink_isolation() {
    let mut s = State::new();

    // Primary screen hyperlink
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(bitty_vt::Hyperlink {
            id: Some("id-primary".into()),
            uri: "https://example.com/primary".into(),
        }),
    });
    prints(&mut s, "P");
    let primary_link_id = s.current_hyperlink();
    assert!(primary_link_id.is_some());

    // Switch to alternate screen
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());

    // Alternate screen hyperlink
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(bitty_vt::Hyperlink {
            id: Some("id-alt".into()),
            uri: "https://example.com/alt".into(),
        }),
    });
    prints(&mut s, "A");
    let alt_link_id = s.current_hyperlink();
    assert!(alt_link_id.is_some());

    // Return to primary screen
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert!(!s.alt_screen_active());

    // Verify cell on primary screen still holds primary hyperlink
    let primary_cell = s.screens.main.get(0, 0);
    assert_eq!(primary_cell.hyperlink, primary_link_id);
    assert!(s.check_invariants().is_ok());
}

// ------------------------------------------------------------------
// M1-18 shell-integration anchors (CTX-0665): zone buffer anchoring,
// prune/clear/reset/reflow invalidation, and prompt-jump primitives.
// ------------------------------------------------------------------

fn feed_anchor_line(state: &mut State, text: &str) {
    prints(state, text);
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
}

fn mark_prompt(state: &mut State) {
    state.apply(&TerminalAction::OscPromptMark {
        kind: ZoneKind::PromptStart,
        exit_code: None,
    });
}

fn history_line_text(state: &State, row: usize) -> String {
    let line = state.scrollback_line(row).expect("history line");
    line.cells
        .iter()
        .filter(|c| !c.spacer)
        .map(|c| c.glyph)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn zone_anchor_resolves_live_cursor_row() {
    let mut s = State::new();
    prints(&mut s, "prompt$ ");
    mark_prompt(&mut s);
    assert_eq!(s.zone_len(), 1);
    let rec = s.zones().next().copied().expect("record");
    let expect = s.scrollback_len() + s.cursor().position.row as usize;
    assert_eq!(s.zone_buffer_row(&rec), Some(expect));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_tracks_line_into_scrollback() {
    let mut s = State::new();
    // A shell emits `OSC 133;A` with the cursor at the prompt row, then
    // prints the prompt: mark first, then print on the same row.
    mark_prompt(&mut s);
    prints(&mut s, "first-prompt$");
    let rec = s.zones().next().copied().expect("record");
    let h = s.height();
    feed_anchor_line(&mut s, "");
    for i in 0..(h + 3) {
        feed_anchor_line(&mut s, &format!("filler{i:02}"));
    }
    assert!(s.scrollback_len() > 0, "marked row must have scrolled");
    let row = s.zone_buffer_row(&rec).expect("anchor survives scroll");
    assert!(row < s.scrollback_len(), "marked row is now history");
    assert_eq!(history_line_text(&s, row), "first-prompt$");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_prune_invalidates_and_jump_skips() {
    let mut s = State::with_scrollback_lines(4);
    prints(&mut s, "doomed-prompt$");
    mark_prompt(&mut s);
    let rec = s.zones().next().copied().expect("record");
    assert!(s.zone_buffer_row(&rec).is_some());
    // Churn past both the grid height (so lines actually scroll) and the
    // tiny retention cap (so the marked line is pruned).
    let h = s.height();
    for i in 0..(h + 16) {
        feed_anchor_line(&mut s, &format!("churn{i:02}"));
    }
    // The record is retained (zone cap is 1024) but its line is pruned.
    assert_eq!(s.zone_len(), 1);
    assert_eq!(
        s.zone_buffer_row(&rec),
        None,
        "pruned anchor must fail closed"
    );
    let total = s.scrollback_len() + s.height();
    assert_eq!(s.prev_prompt_buffer_row(total), None);
    assert_eq!(s.next_prompt_buffer_row(0), None);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_clear_invalidates_but_keeps_record() {
    let mut s = State::new();
    prints(&mut s, "prompt$");
    mark_prompt(&mut s);
    let rec = s.zones().next().copied().expect("record");
    assert!(s.zone_buffer_row(&rec).is_some());
    let epoch_before = s.buffer_epoch();
    s.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::Scrollback,
    });
    assert!(s.buffer_epoch() > epoch_before, "clear bumps the epoch");
    // The record survives (unlike FullReset) but no longer resolves:
    // without the epoch check it could land on unrelated new content.
    assert_eq!(s.zone_len(), 1);
    assert_eq!(s.zone_buffer_row(&rec), None);
    // New marks after the clear resolve again.
    prints(&mut s, "fresh$");
    mark_prompt(&mut s);
    let fresh = s.zones().copied().last().expect("record");
    assert!(s.zone_buffer_row(&fresh).is_some());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_full_reset_drops_zones() {
    let mut s = State::new();
    prints(&mut s, "prompt$");
    mark_prompt(&mut s);
    assert_eq!(s.zone_len(), 1);
    s.apply(&TerminalAction::FullReset);
    assert_eq!(s.zone_len(), 0);
}

#[test]
fn zone_anchor_resize_invalidates_but_noop_keeps() {
    let mut s = State::new();
    prints(&mut s, "prompt$");
    mark_prompt(&mut s);
    let rec = s.zones().next().copied().expect("record");
    let (w, h) = (s.width(), s.height());
    s.resize(w, h);
    assert!(
        s.zone_buffer_row(&rec).is_some(),
        "no-op resize keeps anchors"
    );
    s.resize(w, h + 2);
    assert_eq!(
        s.zone_buffer_row(&rec),
        None,
        "geometry change reassigns rows"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_alt_screen_mismatch_fails_closed() {
    let mut s = State::new();
    prints(&mut s, "main-prompt$");
    mark_prompt(&mut s);
    let main_rec = s.zones().next().copied().expect("record");
    assert!(s.zone_buffer_row(&main_rec).is_some());
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    assert_eq!(
        s.zone_buffer_row(&main_rec),
        None,
        "main-screen mark must not resolve on alt"
    );
    prints(&mut s, "alt-prompt$");
    mark_prompt(&mut s);
    let alt_rec = s.zones().copied().last().expect("record");
    assert!(s.zone_buffer_row(&alt_rec).is_some());
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert!(!s.alt_screen_active());
    assert!(s.zone_buffer_row(&main_rec).is_some());
    assert_eq!(
        s.zone_buffer_row(&alt_rec),
        None,
        "alt-screen mark must not resolve on main"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn prompt_jump_prev_next_strict_and_ordered() {
    let mut s = State::new();
    prints(&mut s, "cmd-one$");
    mark_prompt(&mut s);
    feed_anchor_line(&mut s, "output one");
    prints(&mut s, "cmd-two$");
    mark_prompt(&mut s);
    feed_anchor_line(&mut s, "output two");
    prints(&mut s, "cmd-three$");
    mark_prompt(&mut s);
    let rows: Vec<usize> = s
        .zones()
        .copied()
        .collect::<Vec<_>>()
        .iter()
        .map(|r| s.zone_buffer_row(r).expect("all live"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert!(rows[0] < rows[1] && rows[1] < rows[2]);
    let total = s.scrollback_len() + s.height();
    assert_eq!(s.prev_prompt_buffer_row(total), Some(rows[2]));
    assert_eq!(s.prev_prompt_buffer_row(rows[2]), Some(rows[1]));
    // Strictly before: exactly on a prompt skips it.
    assert_eq!(s.prev_prompt_buffer_row(rows[1]), Some(rows[0]));
    assert_eq!(s.prev_prompt_buffer_row(rows[0]), None);
    // `from` is strict: a prompt exactly at `from` is skipped.
    assert_eq!(s.next_prompt_buffer_row(0), Some(rows[1]));
    assert_eq!(s.next_prompt_buffer_row(rows[0]), Some(rows[1]));
    assert_eq!(s.next_prompt_buffer_row(rows[1]), Some(rows[2]));
    assert_eq!(s.next_prompt_buffer_row(rows[2]), None);
    assert!(s.check_invariants().is_ok());
}

fn mark_zone(state: &mut State, kind: ZoneKind) {
    state.apply(&TerminalAction::OscPromptMark {
        kind,
        exit_code: None,
    });
}

fn mark_output_end(state: &mut State, exit_code: i32) {
    state.apply(&TerminalAction::OscPromptMark {
        kind: ZoneKind::OutputEnd,
        exit_code: Some(exit_code),
    });
}

/// One canonical shell-traffic command block: `A` prompt, command line,
/// `C` output start, output lines, `D` exit status, `A` next prompt.
///
/// Lines end with CR+LF like real shell traffic: bare LF keeps the column,
/// so a CR is required for each new line to start at column zero.
fn feed_shell_line(state: &mut State, text: &str) {
    prints(state, text);
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
    state.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
}

fn feed_command(state: &mut State, cmd: &str, output: &[&str], exit_code: i32) {
    mark_zone(state, ZoneKind::PromptStart);
    prints(state, cmd);
    feed_shell_line(state, "");
    mark_zone(state, ZoneKind::InputStart);
    prints(state, cmd);
    feed_shell_line(state, "");
    mark_zone(state, ZoneKind::OutputStart);
    for line in output {
        feed_shell_line(state, line);
    }
    mark_output_end(state, exit_code);
}

fn live_line_text(state: &State, row: usize) -> String {
    state
        .live_grid_row(row)
        .expect("live row")
        .iter()
        .filter(|c| !c.spacer)
        .map(|c| c.glyph)
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn command_output_range_completed_command() {
    let mut s = State::new();
    feed_command(&mut s, "cmd-one", &["out1", "out2"], 0);
    prints(&mut s, "next$");
    let (start, end) = s.last_command_output_rows().expect("completed range");
    assert_eq!(end - start + 1, 2, "exactly the two output lines");
    assert_eq!(live_line_text(&s, start), "out1");
    assert_eq!(live_line_text(&s, end), "out2");
    // The closing `D` row holds the next prompt, never the output.
    assert_eq!(live_line_text(&s, end + 1), "next$");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_last_command_wins() {
    let mut s = State::new();
    feed_command(&mut s, "cmd-one", &["old-output"], 0);
    prints(&mut s, "mid$");
    feed_command(&mut s, "cmd-two", &["new-a", "new-b", "new-c"], 1);
    prints(&mut s, "next$");
    let (start, end) = s.last_command_output_rows().expect("latest range");
    assert_eq!(end - start + 1, 3);
    assert_eq!(live_line_text(&s, start), "new-a");
    assert_eq!(live_line_text(&s, end), "new-c");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_empty_output_is_none() {
    let mut s = State::new();
    mark_zone(&mut s, ZoneKind::PromptStart);
    prints(&mut s, "true");
    feed_anchor_line(&mut s, "");
    mark_zone(&mut s, ZoneKind::OutputStart);
    // `D` on the same row: zero-byte output, no selectable range.
    mark_output_end(&mut s, 0);
    prints(&mut s, "next$");
    assert_eq!(s.last_command_output_rows(), None);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_running_closes_at_cursor() {
    let mut s = State::new();
    mark_zone(&mut s, ZoneKind::PromptStart);
    prints(&mut s, "sleep 9");
    feed_shell_line(&mut s, "");
    mark_zone(&mut s, ZoneKind::OutputStart);
    feed_shell_line(&mut s, "partial");
    // Cursor sits at column zero of a fresh line: that line is excluded.
    let (start, end) = s.last_command_output_rows().expect("running range");
    assert_eq!(live_line_text(&s, start), "partial");
    assert_eq!(start, end);
    // A partial line under the cursor (no trailing newline) is included.
    prints(&mut s, "more");
    let (start2, end2) = s.last_command_output_rows().expect("running range");
    assert_eq!(start2, start);
    assert_eq!(end2, start + 1);
    assert_eq!(live_line_text(&s, end2), "more");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_no_marks_is_none() {
    let mut s = State::new();
    feed_anchor_line(&mut s, "plain output, no integration");
    assert_eq!(s.last_command_output_rows(), None);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_end_without_start_is_none() {
    let mut s = State::new();
    feed_anchor_line(&mut s, "stray output");
    mark_output_end(&mut s, 0);
    assert_eq!(s.last_command_output_rows(), None);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_tracks_output_into_scrollback() {
    let mut s = State::new();
    feed_command(&mut s, "cmd-one", &["kept-a", "kept-b"], 0);
    prints(&mut s, "next$");
    let h = s.height();
    for i in 0..(h + 4) {
        feed_anchor_line(&mut s, &format!("filler{i:02}"));
    }
    assert!(s.scrollback_len() > 0, "output must have scrolled");
    let (start, end) = s.last_command_output_rows().expect("scrolled range");
    assert_eq!(end - start + 1, 2);
    assert!(end < s.scrollback_len(), "range is now history");
    let sb = |row: usize| history_line_text(&s, row);
    assert_eq!(sb(start), "kept-a");
    assert_eq!(sb(end), "kept-b");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn command_output_range_pruned_is_none() {
    let mut s = State::with_scrollback_lines(4);
    feed_command(&mut s, "cmd-one", &["doomed"], 0);
    assert!(s.last_command_output_rows().is_some());
    let h = s.height();
    for i in 0..(h + 16) {
        feed_anchor_line(&mut s, &format!("churn{i:02}"));
    }
    assert_eq!(
        s.last_command_output_rows(),
        None,
        "pruned output must fail closed"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn zone_anchor_hash_deterministic_across_identical_states() {
    let mut a = State::new();
    let mut b = State::new();
    for st in [&mut a, &mut b] {
        prints(st, "prompt$");
        mark_prompt(st);
        feed_anchor_line(st, "output");
    }
    assert_eq!(a.state_hash(), b.state_hash());
    // A scrolled state diverges from a fresh one with the same zone kinds.
    let mut c = State::new();
    prints(&mut c, "prompt$");
    mark_prompt(&mut c);
    assert_ne!(a.state_hash(), c.state_hash());
}

#[test]
fn ctx_0829_width_one_reflow_preserves_wide_char_leads() {
    // CTX-0829: reflow to width=1 must emit width=2 chars as width=1 leads
    // without spacers, preserving grid invariants (no orphan spacers).
    let mut state = State::new();
    prints(&mut state, "X"); // Narrow char first for clear separation
    prints(&mut state, "あ"); // Wide char

    state.resize(1, 24);

    // After resize to width=1: should have separate rows
    let row0 = state.screens.main.get(0, 0);
    let row1 = state.screens.main.get(1, 0);

    // Both should be leads (not spacers)
    assert!(!row0.spacer);
    assert!(!row1.spacer);

    // Wide char should be width=1 after reflow
    if row0.glyph == 'あ' {
        assert_eq!(
            row0.width, 1,
            "Wide char should be width=1 in 1-column grid"
        );
    }
    if row1.glyph == 'あ' {
        assert_eq!(
            row1.width, 1,
            "Wide char should be width=1 in 1-column grid"
        );
    }

    // Invariants must hold
    state.check_invariants().unwrap();
}

fn set_fg(state: &mut State, color: Color) {
    state.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Foreground(color)].into_boxed_slice(),
        },
    });
}

fn set_underline(state: &mut State, color: Color) {
    state.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::UnderlineColor(color)].into_boxed_slice(),
        },
    });
}

#[test]
fn ctx_0821_placeholder_prints_width_one_with_pen() {
    // CTX-0821 (issue #1400): U+10EEEE prints as a width-1 cell carrying
    // the cursor pen (fg names the image id), with diacritics attached.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(42));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    prints(&mut s, "\u{10EEEE}\u{0305}\u{030D}");
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{10EEEE}');
    assert_eq!(snap.cells[0].width, 1);
    assert!(!snap.cells[0].spacer);
    assert_eq!(snap.cells[0].style.foreground, Some(Color::Indexed(42)));
    assert_eq!(
        snap.cells[0].zerowidth.as_slice(),
        &['\u{0305}', '\u{0305}']
    );
    assert_eq!(
        snap.cells[1].zerowidth.as_slice(),
        &['\u{0305}', '\u{030D}']
    );
    assert_eq!(s.cursor().position.col, 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_run_decodes_tiles() {
    // A 2-cell run with explicit row/col diacritics decodes to tiles
    // (0,0)+(0,1) of image 42.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(42));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    prints(&mut s, "\u{10EEEE}\u{0305}\u{030D}");
    let (cells, key) = s.kitty_unicode_run_at(0, 0).expect("run at origin");
    assert_eq!(key, (42, None));
    assert_eq!(cells.len(), 2);
    assert_eq!((cells[0].id.row, cells[0].id.col), (0, 0));
    assert_eq!((cells[1].id.row, cells[1].id.col), (0, 1));
    assert_eq!(cells[0].id.image_id, 42);
    // Querying the second cell resolves the same run.
    let (cells2, key2) = s.kitty_unicode_run_at(0, 1).expect("run at col 1");
    assert_eq!(key2, (42, None));
    assert_eq!(cells2.len(), 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_bare_continuation_inherits() {
    // No diacritics: the column advances past the previous cell (kitty
    // left-to-right inheritance); row-only marks inherit the column.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    prints(&mut s, "\u{10EEEE}");
    let (cells, _) = s.kitty_unicode_run_at(0, 0).expect("bare run");
    assert_eq!(cells.len(), 2);
    assert_eq!((cells[1].id.row, cells[1].id.col), (0, 1));

    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    prints(&mut s, "\u{10EEEE}\u{0305}");
    let (cells, _) = s.kitty_unicode_run_at(0, 0).expect("row-only run");
    assert_eq!((cells[1].id.row, cells[1].id.col), (0, 1));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_run_breaks_on_color_change() {
    // A foreground change starts a new run (different image id).
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    set_fg(&mut s, Color::Indexed(8));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{030D}");
    let runs = s.kitty_unicode_runs_on_row(0);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].1, (7, None));
    assert_eq!(runs[1].1, (8, None));
    assert_eq!(runs[0].0.len(), 1);
    assert_eq!(runs[1].0.len(), 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_high_byte_and_placement_id() {
    // Third diacritic carries the image-id high byte (42 + 2<<24, the
    // kitty spec example); underline color carries the placement id.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(42));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}\u{030E}");
    let (cells, key) = s.kitty_unicode_run_at(0, 0).expect("high-byte run");
    assert_eq!(cells[0].id.image_id, 42 + (2 << 24));
    assert_eq!(key, (42 + (2 << 24), None));

    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(42));
    set_underline(&mut s, Color::Indexed(21));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    let (cells, key) = s.kitty_unicode_run_at(0, 0).expect("placed run");
    assert_eq!(cells[0].id.placement_id, Some(21));
    assert_eq!(key, (42, Some(21)));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_reservation_neighbors_are_plain_text() {
    // Only U+10EEEE is the placeholder; U+10EEEF..=U+10EEFF print as
    // ordinary width-1 glyphs and never start or join a run.
    let mut s = State::new();
    prints(&mut s, "\u{10EEEF}\u{10EEFF}A");
    let snap = s.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{10EEEF}');
    assert_eq!(snap.cells[1].glyph, '\u{10EEFF}');
    assert!(s.kitty_unicode_run_at(0, 0).is_none());
    assert!(s.kitty_unicode_runs_on_row(0).is_empty());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_clear_removes_only_named_runs() {
    // Delete semantics: clearing image 7 removes its run but keeps the
    // neighboring image-8 run; unknown ids clear nothing.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    prints(&mut s, "\u{10EEEE}\u{0305}\u{030D}");
    set_fg(&mut s, Color::Indexed(8));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    assert_eq!(s.kitty_unicode_clear(99, None), 0);
    assert_eq!(s.kitty_unicode_clear(7, None), 2);
    let snap = s.snapshot();
    assert!(snap.cells[0].is_blank());
    assert!(snap.cells[1].is_blank());
    assert_eq!(snap.cells[2].glyph, '\u{10EEEE}');
    assert_eq!(s.kitty_unicode_runs_on_row(0).len(), 1);
    // Placement-scoped clear: Some(p) keeps runs with other placements.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    set_underline(&mut s, Color::Indexed(1));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    set_underline(&mut s, Color::Indexed(2));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    assert_eq!(s.kitty_unicode_clear(7, Some(1)), 1);
    let snap = s.snapshot();
    assert!(snap.cells[0].is_blank());
    assert_eq!(snap.cells[1].glyph, '\u{10EEEE}');
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0821_placeholder_reflow_moves_cells_with_text() {
    // Resize reflow keeps placeholder cells deterministic: they travel
    // with surrounding text through the soft-wrap rewrap (into scrollback
    // when the grid narrows past them), preserving grid invariants.
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(
        &mut s,
        "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}",
    );
    for _ in 0..(GRID_COLUMNS - 2) {
        prints(&mut s, "x");
    }
    assert_eq!(s.kitty_unicode_runs_on_row(0).len(), 1);
    s.resize(40, GRID_ROWS);
    assert!(s.check_invariants().is_ok());
    // The 80-col logical line rewraps to two 40-col rows; the run's
    // cells survive either on the grid or in scrollback.
    let grid_runs: usize = (0..GRID_ROWS)
        .map(|r| s.kitty_unicode_runs_on_row(r).len())
        .sum();
    let sb_cells = s
        .scrollback()
        .flat_map(|line| line.cells.iter())
        .filter(|c| c.glyph == '\u{10EEEE}')
        .count();
    let grid_cells = s
        .snapshot()
        .cells
        .iter()
        .filter(|c| c.glyph == '\u{10EEEE}')
        .count();
    assert_eq!(
        sb_cells + grid_cells,
        2,
        "both placeholder cells survive reflow"
    );
    assert!(grid_runs <= 2);
}

#[test]
fn ctx_0821_placeholder_erase_and_scroll_drop_deterministically() {
    // EL Right over a run erases its cells (runs decode empty after);
    // scrolling the run off the grid moves its bytes into scrollback
    // (immutable history keeps them; the live grid decodes nothing).
    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(
        &mut s,
        "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}",
    );
    s.apply(&TerminalAction::CursorPosition {
        row: Row(1),
        col: Col(1),
    });
    s.apply(&TerminalAction::EraseInLine {
        mode: EraseLineMode::Right,
    });
    assert!(s.kitty_unicode_runs_on_row(0).is_empty());
    assert!(s.check_invariants().is_ok());

    let mut s = State::new();
    set_fg(&mut s, Color::Indexed(7));
    prints(&mut s, "\u{10EEEE}\u{0305}\u{0305}");
    for _ in 0..(GRID_ROWS + 5) {
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    assert!(s.scrollback_len() > 0);
    assert!(s.kitty_unicode_runs_on_row(GRID_ROWS - 1).is_empty());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn ctx_0910_wide_char_on_single_column_grid_drops_instead_of_panic() {
    // CTX-0910: When the terminal width is only 1 column, a wide character
    // (which requires 2 columns) cannot be printed anywhere. It must be
    // dropped rather than causing a panic when trying to write the spacer
    // at col+1.
    let mut s = State::new();
    s.resize(1, 1);
    assert_eq!(s.width(), 1);
    assert_eq!(s.height(), 1);

    // Print a wide character (U+2AAAA '𪪪' takes 2 cells)
    prints(&mut s, "𪪪");

    // Should not panic, and the grid should remain empty since the wide
    // character cannot fit
    let snapshot = s.snapshot();
    assert_eq!(snapshot.cells.len(), 1);
    assert_eq!(snapshot.cells[0].glyph, ' ');
    assert!(s.check_invariants().is_ok());

    // Also test with auto_wrap disabled
    let mut s = State::new();
    s.resize(1, 1);
    s.set_mode(bitty_vt::Mode::AutoWrap, false);
    prints(&mut s, "𪪪");
    let snapshot = s.snapshot();
    assert_eq!(snapshot.cells[0].glyph, ' ');
    assert!(s.check_invariants().is_ok());
}

// -- CTX-0950 (issue #1668): kitty advanced placements, lifetime, animation.

/// Builds a `KittyGraphics` action the way the parser emits one:
/// base fields plus advanced control keys.
fn kitty_graphics(
    action: Option<char>,
    cols: u16,
    rows: u16,
    cursor_move: u8,
    keys: KittyControlKeys,
) -> TerminalAction {
    TerminalAction::KittyGraphics {
        format_f: 0,
        width_s: None,
        height_v: None,
        action_a: action,
        cols_c: cols,
        rows_r: rows,
        cursor_movement_c: cursor_move,
        payload: Box::default(),
        control: keys,
    }
}

fn place_keys(image: u32, placement: u32) -> KittyControlKeys {
    KittyControlKeys {
        image_id: image,
        placement_id: placement,
        ..Default::default()
    }
}

#[test]
fn kitty_place_anchors_at_cursor_and_advances() {
    let mut s = State::new();
    // yazi-style `a=p,i=7,c=3,r=2` at the origin.
    s.apply(&kitty_graphics(Some('p'), 3, 2, 0, place_keys(7, 0)));
    let entry = s.kitty_placements().get(7, 0).expect("placed");
    assert_eq!((entry.anchor_row, entry.anchor_col), (0, 0));
    assert_eq!((entry.cols, entry.rows), (3, 2));
    // Cursor moves past the span (cols right, rows down).
    assert_eq!(s.cursor().position.col, 3);
    assert_eq!(s.cursor().position.row, 2);
    assert!(s.check_invariants().is_ok());
    // `C=1` suppresses cursor motion; re-put replaces in place.
    s.apply(&TerminalAction::CursorPosition {
        row: Row(1),
        col: Col(1),
    });
    let mut keys = place_keys(7, 9);
    keys.quiet = 1;
    s.apply(&kitty_graphics(Some('p'), 3, 2, 1, keys));
    assert_eq!(s.cursor().position.col, 0);
    assert_eq!(s.cursor().position.row, 0);
    assert_eq!(s.kitty_placements().len(), 2);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_virtual_prototype_mixes_with_placeholder_text() {
    let mut s = State::new();
    // `a=p,U=1,i=42,c=2,r=1`: prototype, no anchor, no cursor motion.
    let mut keys = place_keys(42, 0);
    keys.unicode_placement = 1;
    s.apply(&kitty_graphics(Some('p'), 2, 1, 0, keys));
    let proto = s.kitty_placements().get(42, 0).expect("prototype");
    assert!(proto.virtual_proto);
    assert_eq!(s.cursor().position.col, 0);
    // The image then flows as text: two placeholders in fg 42 name tiles
    // (0,0) and (0,1) of the same image via leftward inheritance.
    s.apply(&TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: vec![AttributeChange::Foreground(Color::Indexed(42))].into_boxed_slice(),
        },
    });
    prints(&mut s, "\u{10EEEE}\u{10EEEE}");
    let (cells, key) = s.kitty_unicode_run_at(0, 0).expect("run decodes");
    assert_eq!(key, (42, None));
    assert_eq!(cells.len(), 2);
    assert_eq!((cells[0].id.row, cells[0].id.col), (0, 0));
    assert_eq!((cells[1].id.row, cells[1].id.col), (0, 1));
    // ...while the same image id resolves to the stored prototype.
    assert_eq!(s.kitty_placements().get(42, 0).unwrap().image_id, 42);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_scroll_moves_and_clips_placements() {
    let mut s = State::new();
    s.apply(&TerminalAction::CursorPosition {
        row: Row(21),
        col: Col(1),
    });
    s.apply(&kitty_graphics(Some('p'), 2, 2, 1, place_keys(1, 1)));
    assert_eq!(s.kitty_placements().get(1, 1).unwrap().anchor_row, 20);
    // A virtual prototype has no anchor: scrolling ignores it.
    let mut vkeys = place_keys(3, 0);
    vkeys.unicode_placement = 1;
    s.apply(&kitty_graphics(Some('p'), 2, 1, 1, vkeys));
    s.apply(&TerminalAction::ScrollUp { n: Count(5) });
    assert_eq!(s.kitty_placements().get(1, 1).unwrap().anchor_row, 15);
    assert!(s.kitty_placements().get(3, 0).unwrap().virtual_proto);
    // Scrolling the real placement off the top clips it away.
    s.apply(&TerminalAction::ScrollUp { n: Count(16) });
    assert!(s.kitty_placements().get(1, 1).is_none());
    assert!(s.kitty_placements().get(3, 0).is_some());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_delete_selectors_and_full_clear() {
    let mut s = State::new();
    s.apply(&kitty_graphics(Some('p'), 2, 2, 1, place_keys(5, 1)));
    s.apply(&kitty_graphics(Some('p'), 2, 2, 1, place_keys(6, 2)));
    // `a=d,d=i,i=5` removes only image 5.
    let mut keys = KittyControlKeys {
        delete: Some('i'),
        ..place_keys(5, 0)
    };
    s.apply(&kitty_graphics(Some('d'), 0, 0, 0, keys));
    assert!(s.kitty_placements().get(5, 1).is_none());
    assert!(s.kitty_placements().get(6, 2).is_some());
    // Unknown selectors refuse without deleting.
    keys = KittyControlKeys {
        delete: Some('~'),
        ..Default::default()
    };
    let refused = s.telemetry().kitty_refused;
    s.apply(&kitty_graphics(Some('d'), 0, 0, 0, keys));
    assert!(s.kitty_placements().get(6, 2).is_some());
    assert_eq!(s.telemetry().kitty_refused, refused + 1);
    // `ED 2` full clear drops the rest; plain line erase does not.
    s.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::All,
    });
    assert!(s.kitty_placements().is_empty());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_relative_placement_needs_parent() {
    let mut s = State::new();
    // Missing parent: refused, counted, nothing stored.
    let mut keys = place_keys(9, 2);
    keys.parent_id = 77;
    keys.parent_placement_id = 1;
    let refused = s.telemetry().kitty_refused;
    s.apply(&kitty_graphics(Some('p'), 2, 2, 0, keys));
    assert!(s.kitty_placements().is_empty());
    assert_eq!(s.telemetry().kitty_refused, refused + 1);
    // Parent at the origin, 4x4: child offsets (H=1 right, V=2 down).
    s.apply(&kitty_graphics(Some('p'), 4, 4, 0, place_keys(9, 1)));
    let mut keys = place_keys(9, 2);
    keys.parent_id = 9;
    keys.parent_placement_id = 1;
    keys.parent_dx = 1;
    keys.parent_dy = 2;
    s.apply(&kitty_graphics(Some('p'), 2, 2, 0, keys));
    let child = s.kitty_placements().get(9, 2).expect("child placed");
    assert_eq!((child.anchor_row, child.anchor_col), (2, 1));
    // Relatives never move the cursor (parent's 4x4 put left it at 4,4).
    assert_eq!(s.cursor().position.col, 4);
    assert_eq!(s.cursor().position.row, 4);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_alt_switch_scopes_placements() {
    let mut s = State::new();
    s.apply(&kitty_graphics(Some('p'), 2, 2, 1, place_keys(1, 1)));
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    // ?1049 entry cleared the alt set (empty anyway); put on alt.
    s.apply(&kitty_graphics(Some('p'), 2, 2, 1, place_keys(2, 2)));
    assert_eq!(s.kitty_placements().len(), 2);
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    // Alt placement died with the session; main survived underneath.
    assert!(s.kitty_placements().get(2, 2).is_none());
    assert!(s.kitty_placements().get(1, 1).is_some());
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_animation_control_and_frame_cap() {
    let mut s = State::new();
    // `a=f,i=9` with default gap appends a frame descriptor.
    let keys = place_keys(9, 0);
    s.apply(&kitty_graphics(Some('f'), 0, 0, 0, keys));
    assert_eq!(
        s.kitty_placements()
            .animation(9)
            .expect("anim")
            .frame_count(),
        2
    );
    // `a=a,i=9,c=2` selects frame 2; `c=9` is refused (no such frame).
    s.apply(&TerminalAction::KittyGraphics {
        format_f: 0,
        width_s: None,
        height_v: None,
        action_a: Some('a'),
        cols_c: 2,
        rows_r: 0,
        cursor_movement_c: 0,
        payload: Box::default(),
        control: place_keys(9, 0),
    });
    assert_eq!(s.kitty_placements().animation(9).unwrap().current(), 2);
    let refused = s.telemetry().kitty_refused;
    s.apply(&TerminalAction::KittyGraphics {
        format_f: 0,
        width_s: None,
        height_v: None,
        action_a: Some('a'),
        cols_c: 9,
        rows_r: 0,
        cursor_movement_c: 0,
        payload: Box::default(),
        control: place_keys(9, 0),
    });
    assert_eq!(s.telemetry().kitty_refused, refused + 1);
    // Frame floods fail closed at the cap instead of growing memory.
    for _ in 0..300 {
        s.apply(&kitty_graphics(Some('f'), 0, 0, 0, place_keys(9, 0)));
    }
    assert!(
        s.kitty_placements().animation(9).unwrap().frame_count()
            <= crate::placement::KITTY_ANIM_MAX_FRAMES as u32
    );
    assert!(s.telemetry().kitty_refused > refused + 1);
    // Unknown animation target (no `i=`, no known `I=`) is refused.
    let keys = KittyControlKeys {
        image_number: 4242,
        ..Default::default()
    };
    s.apply(&kitty_graphics(Some('a'), 1, 0, 0, keys));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn kitty_spanless_transmit_records_nothing_headless() {
    // Without `c=`/`r=` spans the grid anchor is unknowable headless
    // (pixels size it downstream): counted, not stored, cursor kept.
    let mut s = State::new();
    let refused = s.telemetry().kitty_refused;
    s.apply(&kitty_graphics(None, 0, 0, 0, KittyControlKeys::default()));
    assert!(s.kitty_placements().is_empty());
    assert_eq!(s.telemetry().kitty_refused, refused + 1);
    assert_eq!((s.cursor().position.row, s.cursor().position.col), (0, 0));
    assert!(s.check_invariants().is_ok());
}
