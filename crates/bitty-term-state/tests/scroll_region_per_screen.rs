//! Regression tests for issue #3259 (CTX-1903): `DECSTBM` margins are
//! primary-screen state (recorded divergence D-1), so a TUI's status-line
//! region no longer survives the alternate screen and clips the shell.
//!
//! Reported defect: `agy` in Swarm mode installs a status-line region inside
//! the alternate screen, presses `Esc`, and the pane freezes. After `?1049l`
//! the primary screen stayed clipped to the TUI's region, so the shell could
//! never scroll again and every prompt landed on one line.
//!
//! Every assertion reads grid truth — which row holds which text after the
//! action sequence — never damage counts.
//!
//! Fixtures use multi-row regions (`CSI 1;5r` on a 24-row grid): xterm's
//! `set_tb_margins` and ghostty both reject `top == bottom`, so a one-row
//! region would model a configuration no reference terminal accepts.

use bitty_term_state::{State, TerminalAction};
use bitty_vt::{Col, ControlChar, GraphemeCell, Mode, Row};

const CR: u8 = 0x0D;
const LF: u8 = 0x0A;
const RI: u8 = 0x8D;

fn print(state: &mut State, text: &str) {
    for ch in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(ch)));
    }
}

/// Places the cursor at a zero-based absolute row, column 0.
fn place(state: &mut State, row: usize) {
    state.apply(&TerminalAction::CursorPosition {
        row: Row(row as u16 + 1),
        col: Col(1),
    });
}

fn control(state: &mut State, byte: u8) {
    state.apply(&TerminalAction::PrintControl(ControlChar(byte)));
}

fn crlf(state: &mut State) {
    control(state, CR);
    control(state, LF);
}

/// Sets `DECSTBM` from zero-based inclusive rows.
fn set_region(state: &mut State, top: usize, bottom: usize) {
    state.apply(&TerminalAction::SetScrollRegion {
        top: Row(top as u16 + 1),
        bottom: Row(bottom as u16 + 1),
    });
}

fn set_alt(state: &mut State, mode: Mode, enabled: bool) {
    state.apply(&TerminalAction::SetMode { mode, enabled });
}

/// One-distinct-label-per-row markers (`<prefix><row>`), written in place so
/// the fill itself never scrolls.
fn fill_rows(state: &mut State, prefix: &str) -> Vec<String> {
    let height = state.height();
    let labels: Vec<String> = (0..height).map(|row| format!("{prefix}{row}")).collect();
    for (row, label) in labels.iter().enumerate() {
        place(state, row);
        print(state, label);
    }
    labels
}

/// Grid truth: one trimmed string per screen row, top to bottom.
fn rows(state: &State) -> Vec<String> {
    let snap = state.snapshot();
    let width = snap.width;
    (0..snap.height)
        .map(|row| {
            (0..width)
                .map(|col| snap.cells[row * width + col].glyph)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn labels(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|row| format!("{prefix}{row}")).collect()
}

// ----------------------------------------------------------------------
// Margin leak in both directions (the reported defect)
// ----------------------------------------------------------------------

#[test]
fn alt_screen_status_region_does_not_leak_into_primary_after_exit() {
    let mut s = State::new();
    let height = s.height();

    // The shell fills the screen, then a full-screen TUI takes the alternate
    // screen and installs a five-row status-panel region (`CSI 1;5r`).
    fill_rows(&mut s, "main-");
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    set_region(&mut s, 0, 4);
    print(&mut s, "status");
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);

    // The shell prints more lines than fit. The whole primary screen must
    // scroll; before the fix the leftover TUI region clipped it, so rows
    // 0..=22 kept the pre-TUI content forever.
    let last = height + 2;
    for line in 0..=last {
        print(&mut s, &format!("after-{line}"));
        if line < last {
            crlf(&mut s);
        }
    }

    let expected: Vec<String> = (3..=last).map(|line| format!("after-{line}")).collect();
    assert_eq!(rows(&s), expected, "primary screen is still clipped");
    assert_eq!(rows(&s)[height - 1], format!("after-{last}"));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn primary_status_region_does_not_clip_the_alternate_screen() {
    let mut s = State::new();
    let height = s.height();

    // A status-panel region installed on the primary screen (a shell prompt
    // decoration, or a previous TUI that never reset its margins) must not
    // clip the next full-screen application.
    set_region(&mut s, 0, 4);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);

    let last = height - 1;
    for line in 0..=last {
        print(&mut s, &format!("alt-{line}"));
        if line < last {
            crlf(&mut s);
        }
    }

    assert_eq!(rows(&s), labels("alt-", height));
    assert!(s.check_invariants().is_ok());
}

#[test]
fn primary_margins_are_restored_after_an_alternate_screen_session() {
    let mut s = State::new();

    set_region(&mut s, 5, 10);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    // The TUI installs its own, different region.
    set_region(&mut s, 2, 4);
    print(&mut s, "tui");
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);

    // The primary screen owns 5..=10 again: indexing on row 10 scrolls that
    // span and nothing else.
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 10);
    control(&mut s, LF);

    let mut expected = markers.clone();
    expected[5..=9].clone_from_slice(&markers[6..=10]);
    expected[10].clear();
    assert_eq!(rows(&s), expected, "primary scroll region was not restored");
    assert!(s.check_invariants().is_ok());
}

// ----------------------------------------------------------------------
// `?47` vs `?1049` entry and exit
// ----------------------------------------------------------------------

#[test]
fn alternate_screen_entry_starts_from_full_margins_and_47_exit_restores_primary() {
    let mut s = State::new();
    let height = s.height();

    set_region(&mut s, 3, 7);
    set_alt(&mut s, Mode::AlternateScreen, true);

    // `?47` entry starts from full margins even though the primary screen is
    // clipped to 3..=7: indexing on the last row scrolls the whole screen.
    let alt = fill_rows(&mut s, "a");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&alt[1..]);
    assert_eq!(rows(&s), expected, "?47 entry inherited primary margins");

    set_alt(&mut s, Mode::AlternateScreen, false);
    // `?47` exit restores the primary margins (`?47` keeps the cursor, so the
    // cursor stays where the alternate screen left it).
    let markers = fill_rows(&mut s, "p");
    place(&mut s, 7);
    control(&mut s, LF);
    let mut expected = markers.clone();
    expected[3..=6].clone_from_slice(&markers[4..=7]);
    expected[7].clear();
    assert_eq!(
        rows(&s),
        expected,
        "?47 exit did not restore primary margins"
    );
    assert!(s.check_invariants().is_ok());
}

#[test]
fn alternate_screen_47_reentry_starts_from_full_margins() {
    let mut s = State::new();
    let height = s.height();

    set_alt(&mut s, Mode::AlternateScreen, true);
    set_region(&mut s, 2, 4);
    place(&mut s, 0);
    print(&mut s, "altmarker");
    set_alt(&mut s, Mode::AlternateScreen, false);

    // `?47` keeps the alternate grid, but the margins are the previous
    // session's policy: the retained rows survive, the region does not.
    set_alt(&mut s, Mode::AlternateScreen, true);
    assert_eq!(rows(&s)[0], "altmarker", "?47 must retain the alt grid");

    let alt = fill_rows(&mut s, "a");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&alt[1..]);
    assert_eq!(rows(&s), expected, "?47 re-entry inherited alt margins");
}

#[test]
fn alternate_screen_1049_entry_starts_from_full_margins() {
    let mut s = State::new();
    let height = s.height();

    set_region(&mut s, 1, 5);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    // `?1049` clears the alt grid on entry, so the leftover primary region
    // would clip every line the TUI prints.
    let alt = fill_rows(&mut s, "a");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&alt[1..]);
    assert_eq!(rows(&s), expected, "?1049 entry inherited primary margins");
}

#[test]
fn alternate_screen_1049_reentry_starts_from_full_margins() {
    let mut s = State::new();
    let height = s.height();

    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    set_region(&mut s, 2, 4);
    place(&mut s, 3);
    print(&mut s, "tui");
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);

    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    assert!(
        rows(&s).iter().all(|row| row.is_empty()),
        "?1049 re-entry must clear the alt grid"
    );

    let alt = fill_rows(&mut s, "a");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&alt[1..]);
    assert_eq!(rows(&s), expected, "?1049 re-entry kept alt margins");
}

#[test]
fn mixed_47_entry_1049_exit_restores_primary_margins_without_the_cursor() {
    let mut s = State::new();

    set_region(&mut s, 3, 7);
    set_alt(&mut s, Mode::AlternateScreen, true);
    // `?47` entry saves neither the cursor nor a `?1049` pair, but it does
    // snapshot the primary set the margins belong to.
    set_region(&mut s, 10, 14);
    place(&mut s, 20);

    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);
    // Mixed pair: the margins come back (they belong to the screen), the
    // cursor does not (only a `?1049` entry/exit pair restores it).
    assert_eq!(
        s.cursor().position.row,
        20,
        "mixed ?47h/?1049l must not restore the cursor"
    );

    let markers = fill_rows(&mut s, "p");
    place(&mut s, 7);
    control(&mut s, LF);
    let mut expected = markers.clone();
    expected[3..=6].clone_from_slice(&markers[4..=7]);
    expected[7].clear();
    assert_eq!(
        rows(&s),
        expected,
        "mixed ?47h/?1049l did not restore the primary margins"
    );
    assert!(s.check_invariants().is_ok());
}

// ----------------------------------------------------------------------
// Device-level resets
// ----------------------------------------------------------------------

#[test]
fn ris_drops_the_stashed_primary_margins() {
    let mut s = State::new();
    let height = s.height();

    set_region(&mut s, 5, 10);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    s.apply(&TerminalAction::FullReset);
    assert!(!s.alt_screen_active(), "RIS returns to the primary screen");

    // Nothing of the pre-RIS region survives: the primary screen gets full
    // margins, so indexing on the last row scrolls the whole screen.
    let markers = fill_rows(&mut s, "r");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&markers[1..]);
    assert_eq!(rows(&s), expected, "RIS kept a stashed scroll region");
    assert_eq!(s.scrollback_len(), 1);
    assert!(s.check_invariants().is_ok());
}

#[test]
fn decstr_inside_the_alternate_screen_clears_the_stashed_primary_margins() {
    let mut s = State::new();
    let height = s.height();

    set_region(&mut s, 5, 10);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    set_region(&mut s, 1, 3);
    // DECSTR is a device-scope soft reset: it clears the margins that
    // `?1049l` would otherwise restore, not just the ones in front of it.
    s.apply(&TerminalAction::SoftReset);

    let alt = fill_rows(&mut s, "a");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&alt[1..]);
    assert_eq!(rows(&s), expected, "DECSTR left the alt margins installed");

    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);
    let markers = fill_rows(&mut s, "p");
    place(&mut s, height - 1);
    control(&mut s, LF);
    let mut expected = vec![String::new(); height];
    expected[..height - 1].clone_from_slice(&markers[1..]);
    assert_eq!(
        rows(&s),
        expected,
        "DECSTR did not clear the stashed primary margins"
    );
    assert_eq!(s.scrollback_len(), 1);
    assert!(s.check_invariants().is_ok());
}

// ----------------------------------------------------------------------
// Resize keeps each screen's own region
// ----------------------------------------------------------------------

#[test]
fn primary_margins_survive_a_taller_resize_on_the_alternate_screen() {
    let mut s = State::new();

    set_region(&mut s, 5, 10);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    s.resize(80, 30);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);
    assert_eq!(s.height(), 30);

    // Rows are absolute: a taller grid leaves the stored 5..=10 region alone.
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 10);
    control(&mut s, LF);
    let mut expected = markers.clone();
    expected[5..=9].clone_from_slice(&markers[6..=10]);
    expected[10].clear();
    assert_eq!(rows(&s), expected, "resize lost the primary scroll region");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn stored_primary_margins_clamp_when_the_grid_shrinks_below_them() {
    let mut s = State::new();

    // 18..=22 does not fit a 20-row grid: the stored bottom margin must be
    // trimmed to the new last row, never left dangling or grown.
    set_region(&mut s, 18, 22);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    s.resize(80, 20);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);
    assert_eq!(s.height(), 20);

    let markers = fill_rows(&mut s, "r");
    place(&mut s, 19);
    control(&mut s, LF);
    let mut expected = markers.clone();
    expected[18] = markers[19].clone();
    expected[19].clear();
    assert_eq!(rows(&s), expected, "stored margins were not clamped");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn resize_while_the_primary_screen_is_active_leaves_full_margins_after_an_alt_exit() {
    let mut s = State::new();

    set_region(&mut s, 18, 22);
    // The primary screen is active during the resize, so TERM-ENG-005 resets
    // its live margins to full; there is nothing stashed at that point.
    s.resize(80, 20);
    assert_eq!(s.height(), 20);

    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, true);
    set_region(&mut s, 1, 3);
    set_alt(&mut s, Mode::AlternateScreenClearAndRestore, false);

    // The 18..=22 region died with the resize, not with the TUI: the exit
    // must not resurrect a partial region the resized screen had given up.
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 19);
    control(&mut s, LF);
    let mut expected = vec![String::new(); 20];
    expected[..19].clone_from_slice(&markers[1..]);
    assert_eq!(
        rows(&s),
        expected,
        "a pre-resize region came back after the alt-screen exit"
    );
    assert_eq!(s.scrollback_len(), 1);
    assert!(s.check_invariants().is_ok());
}

// ----------------------------------------------------------------------
// Index / reverse index: reference behaviour at the edges
// ----------------------------------------------------------------------

#[test]
fn index_on_the_region_bottom_margin_scrolls_only_the_region() {
    let mut s = State::new();

    set_region(&mut s, 5, 10);
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 10);
    control(&mut s, LF);

    let mut expected = markers.clone();
    expected[5..=9].clone_from_slice(&markers[6..=10]);
    expected[10].clear();
    assert_eq!(rows(&s), expected, "region margin path changed");
    assert_eq!(s.scrollback_len(), 0, "region scroll must not capture");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn reverse_index_on_the_region_top_margin_scrolls_only_the_region() {
    let mut s = State::new();

    set_region(&mut s, 5, 10);
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 5);
    control(&mut s, RI);

    let mut expected = markers.clone();
    expected[6..=10].clone_from_slice(&markers[5..=9]);
    expected[5].clear();
    assert_eq!(rows(&s), expected, "region margin path changed");
    assert!(s.check_invariants().is_ok());
}

#[test]
fn index_at_a_screen_edge_outside_the_region_is_discarded() {
    // xterm `xtermIndex`/`RevIndex`, ghostty `index`/`reverseIndex` and kitty
    // `screen_index` clamp at the screen edge: the index is discarded, the
    // rows (including anything the region protects) stay put, and nothing
    // enters scrollback. Bitty matches; this test pins that decision.
    let mut s = State::new();
    set_region(&mut s, 5, 10);
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 23);
    control(&mut s, LF);
    assert_eq!(
        rows(&s),
        markers,
        "a linefeed outside the region must not scroll"
    );
    assert_eq!(s.scrollback_len(), 0);

    let mut s = State::new();
    set_region(&mut s, 5, 10);
    let markers = fill_rows(&mut s, "r");
    place(&mut s, 0);
    control(&mut s, RI);
    assert_eq!(
        rows(&s),
        markers,
        "a reverse index outside the region must not scroll"
    );
    assert!(s.check_invariants().is_ok());
}
