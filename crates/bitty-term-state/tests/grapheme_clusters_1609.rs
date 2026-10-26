//! Extended grapheme clusters as one layout unit (CTX-1609, issue #1767).
//!
//! The print path must charge an extended grapheme cluster to one leading
//! cell (plus its wide spacer) instead of one cell per scalar: ZWJ emoji
//! families, regional-indicator flags, skin-tone modifiers, keycaps,
//! variation selectors, combining marks, and spacing marks. The same file
//! covers the configured ambiguous-width policy and the over-cap
//! degradation, because all three share the width decision.

use bitty_term_state::{
    AmbiguousWidth, Cell, MAX_CLUSTER_SCALARS, Snapshot, State, TerminalAction,
};
use bitty_vt::GraphemeCell;

const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';
const VS15: char = '\u{FE0E}';

fn print_text(state: &mut State, text: &str) {
    for scalar in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(scalar)));
    }
}

fn state_with_text(text: &str) -> State {
    let mut state = State::new();
    print_text(&mut state, text);
    state
}

fn row(snapshot: &Snapshot, row: usize) -> &[Cell] {
    let start = row * snapshot.width;
    &snapshot.cells[start..start + snapshot.width]
}

fn cursor_col(state: &State) -> usize {
    state.snapshot().cursor.position.col as usize
}

#[test]
fn combining_mark_stays_on_one_cell() {
    let state = state_with_text("e\u{0301}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, 'e');
    assert_eq!(cells[0].width, 1);
    assert!(!cells[0].spacer);
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{0301}']);
    assert_eq!(cursor_col(&state), 1, "one layout unit, one column");
    assert!(!cells[1].spacer);
}

#[test]
fn zwj_family_occupies_one_wide_span() {
    // Canonical four-person family: seven scalars, six tail slots, exactly
    // the inline cap.
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    let state = state_with_text(family);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{1F468}');
    assert_eq!(cells[0].width, 2);
    assert!(!cells[0].spacer);
    assert_eq!(
        cells[0].zerowidth.as_slice(),
        &[ZWJ, '\u{1F469}', ZWJ, '\u{1F467}', ZWJ, '\u{1F466}']
    );
    assert!(
        cells[1].spacer,
        "one wide spacer, not one per family member"
    );
    assert_eq!(
        cursor_col(&state),
        2,
        "the whole cluster covers two columns"
    );
    assert!(!cells[2].spacer);
    assert!(cells[2].is_blank());
    // No spacer gap: exactly one trailing half for the cluster.
    assert_eq!(cells.iter().filter(|cell| cell.spacer).count(), 1);
}

#[test]
fn regional_indicator_pair_grows_to_two_cells() {
    let state = state_with_text("\u{1F1EB}\u{1F1F7}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{1F1EB}');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{1F1F7}']);
    assert!(cells[1].spacer);
    assert_eq!(cursor_col(&state), 2);
    // A third indicator starts a new width-1 cluster.
    let mut triple = state;
    print_text(&mut triple, "\u{1F1E9}");
    let snapshot = triple.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[2].glyph, '\u{1F1E9}');
    assert_eq!(cells[2].width, 1);
    assert!(!cells[3].spacer);
    assert_eq!(cursor_col(&triple), 3);
}

#[test]
fn skin_tone_modifier_joins_the_base() {
    let state = state_with_text("\u{1F44D}\u{1F3FD}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{1F44D}');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{1F3FD}']);
    assert!(cells[1].spacer);
    assert_eq!(cursor_col(&state), 2);
}

#[test]
fn keycap_sequence_occupies_two_cells() {
    let state = state_with_text("#\u{FE0F}\u{20E3}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '#');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.as_slice(), &[VS16, '\u{20E3}']);
    assert!(cells[1].spacer);
    assert_eq!(cursor_col(&state), 2);
}

#[test]
fn variation_selector_16_widens_text_default_emoji() {
    let state = state_with_text("\u{2764}\u{FE0F}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{2764}');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.as_slice(), &[VS16]);
    assert!(cells[1].spacer);
    assert_eq!(cursor_col(&state), 2);
}

#[test]
fn variation_selector_15_keeps_text_presentation_narrow() {
    let state = state_with_text("\u{2764}\u{FE0E}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].width, 1);
    assert_eq!(cells[0].zerowidth.as_slice(), &[VS15]);
    assert!(!cells[1].spacer);
    assert_eq!(cursor_col(&state), 1);
}

#[test]
fn lone_zwj_consumes_no_cell() {
    let state = state_with_text("\u{200D}");
    let snapshot = state.snapshot();
    assert_eq!(cursor_col(&state), 0);
    assert!(row(&snapshot, 0)[0].is_blank());
    // A joiner after a base attaches without advancing either.
    let joined = state_with_text("a\u{200D}");
    assert_eq!(cursor_col(&joined), 1);
    assert_eq!(row(&joined.snapshot(), 0)[0].zerowidth.as_slice(), &[ZWJ]);
}

#[test]
fn ambiguous_scalar_obeys_the_configured_policy() {
    let narrow = state_with_text("\u{03B1}");
    let narrow_snapshot = narrow.snapshot();
    let narrow_cells = row(&narrow_snapshot, 0);
    assert_eq!(narrow_cells[0].width, 1);
    assert!(!narrow_cells[1].spacer);
    assert_eq!(cursor_col(&narrow), 1);

    let mut wide = State::new();
    wide.set_ambiguous_width(AmbiguousWidth::Wide);
    print_text(&mut wide, "\u{03B1}");
    let wide_snapshot = wide.snapshot();
    let wide_cells = row(&wide_snapshot, 0);
    assert_eq!(wide_cells[0].width, 2);
    assert!(wide_cells[1].spacer);
    assert_eq!(cursor_col(&wide), 2);
    assert_eq!(wide.ambiguous_width(), AmbiguousWidth::Wide);
}

#[test]
fn cjk_fullwidth_and_halfwidth_widths_are_authoritative() {
    // CJK Unified Ideograph: Wide.
    let cjk = state_with_text("\u{4E2D}");
    assert_eq!(row(&cjk.snapshot(), 0)[0].width, 2);
    assert_eq!(cursor_col(&cjk), 2);
    // Fullwidth Latin: Fullwidth.
    let full = state_with_text("\u{FF21}");
    assert_eq!(row(&full.snapshot(), 0)[0].width, 2);
    // Halfwidth Katakana: Halfwidth, one cell.
    let half = state_with_text("\u{FF61}");
    assert_eq!(row(&half.snapshot(), 0)[0].width, 1);
    assert!(!row(&half.snapshot(), 0)[1].spacer);
    assert_eq!(cursor_col(&half), 1);
}

#[test]
fn cluster_tail_is_bounded_and_marks_over_cap_are_dropped() {
    let mut state = State::new();
    print_text(&mut state, "a");
    for _ in 0..(MAX_CLUSTER_SCALARS + 4) {
        print_text(&mut state, "\u{0301}");
    }
    let snapshot = state.snapshot();
    let cell = row(&snapshot, 0)[0];
    assert_eq!(cell.glyph, 'a');
    assert_eq!(cell.width, 1);
    assert_eq!(cell.zerowidth.len(), MAX_CLUSTER_SCALARS);
    assert_eq!(cursor_col(&state), 1, "dropped marks never advance");
}

#[test]
fn over_cap_printable_continuation_starts_a_fresh_cell() {
    // 12-person ZWJ chain: base + 5 joined pairs fit the tail cap (10 > 8),
    // so the scalars past the cap are laid out as fresh clusters instead of
    // being dropped, and the cursor advances deterministically.
    let mut text = String::from("\u{1F468}");
    for _ in 0..11 {
        text.push(ZWJ);
        text.push('\u{1F468}');
    }
    let state = state_with_text(&text);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{1F468}');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.len(), MAX_CLUSTER_SCALARS);
    // The overflow base is placed as its own layout unit at the cursor.
    assert_eq!(cells[2].glyph, '\u{1F468}');
    assert!(!cells[2].spacer);
    assert!(cursor_col(&state) > 2);
}

#[test]
fn spacing_mark_joins_without_advancing() {
    // Devanagari KA + vowel sign I: one cluster, one column (UAX #29 GB9a).
    let state = state_with_text("\u{0915}\u{093F}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{0915}');
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{093F}']);
    assert_eq!(cells[0].width, 1);
    assert_eq!(cursor_col(&state), 1);
}

#[test]
fn prepend_takes_no_cell_and_never_hides_its_base() {
    // CTX-1609: prepends are zero-width (the generated `Prepend` table feeds
    // both GB9b and `is_zero_width`). Mid-line a prepend attaches to the
    // preceding cell's tail exactly like a combining mark — copy order stays
    // byte-exact — and the base it precedes still owns its own lead cell.
    // Before the width fix the prepend consumed a cell, became the cell
    // glyph, and pushed its base into the hidden tail.
    for (prepend, base) in [
        ('\u{0600}', '1'),         // Arabic number sign + digit
        ('\u{08E2}', '1'),         // Arabic disputed end of ayah + digit
        ('\u{0D4E}', 'a'),         // Malayalam letter dot reph + letter
        ('\u{110BD}', '1'),        // Kaithi number sign + digit
        ('\u{111C2}', 'a'),        // Sharada sign jihvamuliya
        ('\u{11A84}', '\u{0915}'), // Soyombo sign jihvamuliya + Devanagari KA
        ('\u{11F02}', 'a'),        // Kawi sign starting stroke
    ] {
        let mut state = State::new();
        print_text(&mut state, "x");
        print_text(&mut state, &format!("{prepend}{base}"));
        let snapshot = state.snapshot();
        let cells = row(&snapshot, 0);
        assert_eq!(cells[0].glyph, 'x');
        assert_eq!(
            cells[0].zerowidth.as_slice(),
            &[prepend],
            "U+{:04X} attaches without a cell of its own",
            prepend as u32
        );
        assert_eq!(
            cells[1].glyph, base,
            "U+{:04X} must not hide its base",
            prepend as u32
        );
        assert_eq!(cells[1].width, 1, "the base keeps one cell");
        assert!(!cells[2].spacer, "no spacer for a one-cell cluster");
        assert_eq!(cursor_col(&state), 2);
        assert_eq!(collect(&cells[..2]), format!("x{prepend}{base}"));

        // At the line start there is no preceding cell: the prepend is
        // dropped under the same documented rule as a lone combining mark,
        // and the base still owns the lead cell.
        let state = state_with_text(&format!("{prepend}{base}"));
        let snapshot = state.snapshot();
        let cells = row(&snapshot, 0);
        assert_eq!(cells[0].glyph, base);
        assert_eq!(cells[0].width, 1);
        assert_eq!(cursor_col(&state), 1);
    }
}

#[test]
fn lone_regional_indicator_is_one_cell_and_pairs_resume() {
    // A single RI is its own cluster and one cell: no phantom spacer, and a
    // following pair still pairs from parity zero.
    let lone = state_with_text("\u{1F1EB}");
    let lone_snapshot = lone.snapshot();
    let lone_cells = row(&lone_snapshot, 0);
    assert_eq!(lone_cells[0].glyph, '\u{1F1EB}');
    assert_eq!(lone_cells[0].width, 1);
    assert!(!lone_cells[1].spacer);
    assert_eq!(cursor_col(&lone), 1);

    let text = "\u{1F1EB}\u{1F1F7}\u{1F1EB}\u{1F1EE}";
    let state = state_with_text(text);
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{1F1EB}');
    assert_eq!(cells[0].width, 2);
    assert!(cells[1].spacer);
    assert_eq!(cells[2].glyph, '\u{1F1EB}');
    assert_eq!(cells[2].width, 2);
    assert!(cells[3].spacer);
    assert_eq!(cursor_col(&state), 4);
}

#[test]
fn wide_ambiguous_cluster_widens_with_its_marks() {
    // Under `ambiguous_width = wide` a cluster led by an ambiguous scalar
    // owns two columns and keeps its mark on the lead cell.
    let mut state = State::new();
    state.set_ambiguous_width(AmbiguousWidth::Wide);
    print_text(&mut state, "\u{03B1}\u{0301}");
    let snapshot = state.snapshot();
    let cells = row(&snapshot, 0);
    assert_eq!(cells[0].glyph, '\u{03B1}');
    assert_eq!(cells[0].width, 2);
    assert_eq!(cells[0].zerowidth.as_slice(), &['\u{0301}']);
    assert!(cells[1].spacer);
    assert_eq!(cursor_col(&state), 2);
}

#[test]
fn cluster_at_the_last_column_wraps_as_one_unit() {
    // A cluster starting in the final column must not split: the state
    // layer places the lead in the last column, keeps the continuation on
    // that cell, and latches the wrap for the next printable scalar.
    let mut state = State::new();
    print_text(&mut state, &"a".repeat(79));
    print_text(&mut state, "\u{0915}\u{093F}");
    let snapshot = state.snapshot();
    let last = row(&snapshot, 0)[79];
    assert_eq!(last.glyph, '\u{0915}');
    assert_eq!(last.zerowidth.as_slice(), &['\u{093F}']);
    assert_eq!(last.width, 1);
    assert!(!last.spacer);
    assert_eq!(
        cursor_col(&state),
        79,
        "the deferred wrap keeps the cursor on the last column"
    );

    // The next printable scalar consumes the latched wrap and starts the
    // next row on column 0.
    print_text(&mut state, "b");
    let snapshot = state.snapshot();
    assert_eq!(row(&snapshot, 1)[0].glyph, 'b');
    assert_eq!(cursor_col(&state), 1);
}

fn collect(cells: &[Cell]) -> String {
    let mut out = String::new();
    for cell in cells {
        cell.push_text_into(&mut out);
    }
    out
}
