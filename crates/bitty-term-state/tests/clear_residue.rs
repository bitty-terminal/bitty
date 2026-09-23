//! `clear` leaves zero styled residue (issue #1337).
//!
//! Headless grid-truth companion to the live grim check: after the exact
//! bytes `clear` emits (`ESC[H ESC[2J ESC[3J` on `xterm-256color`), every
//! grid cell must be blank and unstyled and the scrollback must be empty —
//! regardless of what the previous output contained (plain SGR text, wide
//! / powerline / emoji / combining marks, soft-wrapped lines, or output
//! produced under a scroll region with origin mode).

use bitty_term_state::{AttributeChange, AttributeDiff, Color, State, TerminalAction};
use bitty_vt::{Col, EraseDisplayMode, GraphemeCell, Mode, Row};

fn print_char(c: char) -> TerminalAction {
    TerminalAction::Print(GraphemeCell::from(c))
}

fn set_attrs(changes: Vec<AttributeChange>) -> TerminalAction {
    TerminalAction::SetAttributes {
        attrs: AttributeDiff {
            changes: changes.into_boxed_slice(),
        },
    }
}

fn styled(changes_fg_bg: (u8, u8)) -> TerminalAction {
    set_attrs(vec![
        AttributeChange::Foreground(Color::Indexed(changes_fg_bg.0)),
        AttributeChange::Background(Color::Indexed(changes_fg_bg.1)),
    ])
}

/// The exact `clear` sequence: home + erase-all + erase-scrollback.
fn clear(state: &mut State) {
    state.apply(&TerminalAction::CursorPosition {
        row: Row(1),
        col: Col(1),
    });
    state.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::All,
    });
    state.apply(&TerminalAction::EraseInDisplay {
        mode: EraseDisplayMode::Scrollback,
    });
}

fn assert_clean_grid(state: &State, ctx: &str) {
    let snap = state.snapshot();
    let w = snap.width;
    let mut first: Vec<String> = Vec::new();
    let mut bad = 0usize;
    for (i, cell) in snap.cells.iter().enumerate() {
        let styled = cell.style.foreground.is_some() || cell.style.background.is_some();
        if !cell.is_blank() || styled {
            bad += 1;
            if first.len() < 10 {
                first.push(format!(
                    "row={} col={} glyph={:?} style={:?}",
                    i / w,
                    i % w,
                    cell.glyph,
                    cell.style
                ));
            }
        }
    }
    assert_eq!(
        bad,
        0,
        "{ctx}: zero styled residue must survive clear, got {bad}: {}",
        first.join("; ")
    );
    assert_eq!(
        state.scrollback_len(),
        0,
        "{ctx}: ED 3 must clear scrollback"
    );
}

#[test]
fn clear_leaves_zero_styled_residue_after_sgr_block() {
    // Previous output with a pink-background/green-foreground block at the
    // bottom-left (the reported screenshot), then `clear`.
    let mut s = State::new();
    let h = s.height();
    s.apply(&TerminalAction::CursorPosition {
        row: Row(h as u16),
        col: Col(1),
    });
    s.apply(&styled((2, 13)));
    for c in "XY".chars() {
        s.apply(&print_char(c));
    }
    s.apply(&set_attrs(vec![AttributeChange::Reset]));
    clear(&mut s);
    assert_clean_grid(&s, "sgr block");
}

#[test]
fn clear_leaves_zero_residue_after_wide_and_powerline_output() {
    // Starship-style output: CJK wide chars, nerd-font PUA separators,
    // emoji ZWJ runs, combining marks, SGR-styled, scrolled into history.
    let mut s = State::new();
    let h = s.height();
    let line: Vec<char> = "❯\u{e0b0}\u{6F22}\u{6F22}e\u{301}\u{1F468}\u{200D}\u{1F4BB}~"
        .chars()
        .collect();
    for i in 0..(h + 5) {
        s.apply(&styled(((i % 8) as u8, 13)));
        for c in &line {
            s.apply(&print_char(*c));
        }
        s.apply(&print_char('\n'));
    }
    s.apply(&set_attrs(vec![AttributeChange::Reset]));
    clear(&mut s);
    assert_clean_grid(&s, "wide/powerline output");
}

#[test]
fn clear_leaves_zero_residue_after_wrapped_region_output() {
    // Soft-wrapped styled lines plus output inside a DECSTBM scroll region
    // with origin mode (fullscreen-app leftovers), then `clear`.
    let mut s = State::new();
    let h = s.height();
    for i in 0..(h + 10) {
        s.apply(&styled(((i % 8) as u8, 13)));
        for c in "WRAP0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!@#$".chars() {
            s.apply(&print_char(c));
        }
        s.apply(&print_char('\n'));
    }
    s.apply(&TerminalAction::SetScrollRegion {
        top: Row(5),
        bottom: Row(20),
    });
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: true,
    });
    for _ in 0..30 {
        for c in "region-scroll-line-with-style-0123456789".chars() {
            s.apply(&print_char(c));
        }
        s.apply(&print_char('\n'));
    }
    s.apply(&TerminalAction::SetMode {
        mode: Mode::Origin,
        enabled: false,
    });
    s.apply(&set_attrs(vec![AttributeChange::Reset]));
    clear(&mut s);
    assert_clean_grid(&s, "wrapped/region output");
}
