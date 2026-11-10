//! Regression coverage for #2529: resizing panels must never corrupt or
//! collapse terminal output.
//!
//! The cases mirror the owner repro and the resize contract:
//! - height-only shrink keeps the newest rows and round-trips through
//!   scrollback (including a transient one-row viewport during a drag);
//! - width changes rewrap logical lines idempotently, preserving spaces at
//!   soft-wrap boundaries, wide glyphs, and combining marks;
//! - image placements follow height changes and are cleared safely (with
//!   virtual prototypes retained) when a width reflow invalidates anchors.
//!
//! Headless, deterministic, no I/O.

#![forbid(unsafe_code)]

use bitty_term_state::{KittyControlKeys, Mode, State, TerminalAction};
use bitty_vt::{BoundedString, Col, ControlChar, GraphemeCell, Hyperlink, Row};

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

fn control(state: &mut State, byte: u8) {
    state.apply(&TerminalAction::PrintControl(ControlChar(byte)));
}

fn newline(state: &mut State) {
    control(state, 0x0D);
    control(state, 0x0A);
}

/// One addressable physical row, spacers skipped, trailing blanks trimmed.
fn row_text(cells: &[bitty_term_state::Cell]) -> String {
    let mut row = String::new();
    for cell in cells {
        if cell.spacer {
            continue;
        }
        row.push(cell.glyph);
        for mark in cell.zerowidth.iter() {
            row.push(*mark);
        }
    }
    row.trim_end().to_string()
}

/// Every retained physical row, oldest first: scrollback then live grid.
fn physical_rows(state: &State) -> Vec<(Vec<bitty_term_state::Cell>, bool)> {
    let mut rows = Vec::new();
    for line in state.scrollback() {
        rows.push((line.cells.to_vec(), line.wrapped));
    }
    let snap = state.snapshot();
    for r in 0..snap.height {
        let start = r * snap.width;
        rows.push((
            snap.cells[start..start + snap.width].to_vec(),
            state.row_soft_wrapped(r),
        ));
    }
    rows
}

/// Full addressable text (all physical rows, one per line).
fn addressable_text(state: &State) -> String {
    let mut out = String::new();
    for (cells, _) in physical_rows(state) {
        out.push_str(&row_text(&cells));
        out.push('\n');
    }
    out
}

/// Rebuilds logical lines by joining soft-wrapped rows. Unlike
/// `addressable_text`, intermediate rows keep their trailing spaces so a
/// boundary space is visible.
fn logical_text(state: &State) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for (cells, wrapped) in physical_rows(state) {
        // Layout/erase padding is not text: mirror the production unwrap
        // and drop trailing padding cells from each physical row.
        let mut end = cells.len();
        while end > 0 {
            let cell = &cells[end - 1];
            if cell.padding && cell.is_blank() {
                end -= 1;
            } else {
                break;
            }
        }
        for cell in &cells[..end] {
            if !cell.spacer {
                line.push(cell.glyph);
                for mark in cell.zerowidth.iter() {
                    line.push(*mark);
                }
            }
        }
        if !wrapped {
            out.push_str(line.trim_end());
            out.push('\n');
            line.clear();
        }
    }
    if !line.is_empty() {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn assert_no_orphan_wides(state: &State) {
    for (cells, _) in physical_rows(state) {
        for (i, cell) in cells.iter().enumerate() {
            if cell.spacer {
                assert!(i > 0, "spacer at column 0");
                assert_eq!(cells[i - 1].width, 2, "spacer without a wide lead");
                assert!(!cells[i - 1].spacer);
            } else if cell.width == 2 {
                assert!(i + 1 < cells.len(), "wide lead at the last column");
                assert!(cells[i + 1].spacer, "wide lead without its spacer");
            }
        }
    }
}

fn assert_each_once_in_order(text: &str, needles: &[String]) {
    let mut cursor = 0usize;
    for needle in needles {
        let found = text[cursor..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing after {cursor} in {text:?}"));
        assert_eq!(
            text.matches(needle).count(),
            1,
            "{needle:?} must appear exactly once: {text:?}"
        );
        cursor += found + needle.len();
    }
}

#[test]
fn height_only_shrink_keeps_newest_output_and_roundtrips() {
    let mut s = State::new();
    let mut lines = Vec::new();
    for i in 0..100 {
        let line = format!("line-{i:03}");
        prints(&mut s, &line);
        newline(&mut s);
        lines.push(line);
    }
    assert!(s.check_invariants().is_ok());

    s.resize(80, 10);
    assert_eq!((s.width(), s.height()), (80, 10));
    assert!(s.check_invariants().is_ok());
    assert!(
        s.scrollback_len() >= 20,
        "height shrink must move rows into scrollback, got {}",
        s.scrollback_len()
    );
    // The newest line stays visible in the viewport (the cursor's blank row
    // is naturally last): a shrink never truncates the live output tail.
    let after_shrink = addressable_text(&s);
    assert!(
        after_shrink.contains("line-099"),
        "newest output must remain visible after shrink: {after_shrink:?}"
    );
    assert_each_once_in_order(&after_shrink, &lines);

    // Round-trip: every logical line comes back, one line per row.
    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    let restored = addressable_text(&s);
    assert_each_once_in_order(&restored, &lines);
    let seen: Vec<&str> = restored
        .lines()
        .filter(|row| row.starts_with("line-"))
        .collect();
    for row in &seen {
        assert_eq!(
            row.split_whitespace().count(),
            1,
            "rows must not merge logical lines: {row:?}"
        );
    }
    assert_eq!(
        seen,
        lines.iter().map(String::as_str).collect::<Vec<_>>(),
        "height round-trip must restore line order"
    );
}

#[test]
fn transient_one_row_height_never_squashes_output() {
    let mut s = State::new();
    for i in 0..30 {
        prints(&mut s, &format!("out-{i:02}"));
        newline(&mut s);
    }
    // A divider drag can momentarily solve a pane to a single row. That
    // resize must preserve the tail in scrollback, not collapse it.
    s.resize(80, 1);
    assert_eq!(s.height(), 1);
    assert!(s.check_invariants().is_ok());
    assert!(
        s.scrollback_len() >= 29,
        "the 1-row viewport must capture the rest of the rows"
    );
    let squeezed = addressable_text(&s);
    assert!(
        squeezed.contains("out-29"),
        "the newest row must stay visible: {squeezed:?}"
    );

    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    let restored = addressable_text(&s);
    // No merging: every numbered line occupies its own row again.
    let rows: Vec<&str> = restored.lines().collect();
    for i in 0..30 {
        let needle = format!("out-{i:02}");
        assert!(
            rows.iter().any(|row| *row == needle),
            "{needle} must be its own row after the transient 1-row resize: {restored:?}"
        );
    }
    assert!(
        !restored.contains("out-00out-01"),
        "rows must never be concatenated into one line: {restored:?}"
    );
}

#[test]
fn top_aligned_content_roundtrips_without_history_drift() {
    let mut s = State::new();
    prints(&mut s, "alpha\r\nbeta\r\ngamma\r\n");
    s.resize(80, 8);
    let shrunk = addressable_text(&s);
    let rows: Vec<&str> = shrunk.lines().collect();
    assert_eq!(&rows[0..3], &["alpha", "beta", "gamma"]);
    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    let grown = addressable_text(&s);
    let rows: Vec<&str> = grown.lines().collect();
    assert_eq!(
        &rows[0..3],
        &["alpha", "beta", "gamma"],
        "unused viewport tail must not pull history under top-aligned content"
    );
}

/// Issue #2788: a shrink that absorbs part of the unused bottom padding and
/// scrolls part of the content out must round-trip exactly. The grow restores
/// the history above the grid and pads the absorbed rows *below* it; filling
/// that remainder above the pulled block shifted content down on every
/// shrink/grow cycle, which is what accumulated blank rows at the top of a
/// Mod+drag-moved pane.
#[test]
fn partial_padding_absorb_roundtrips_without_top_blank_rows() {
    let mut s = State::new();
    for i in 1..=10 {
        prints(&mut s, &format!("line-{i:02}"));
        newline(&mut s);
    }
    // 24 rows: 10 content rows plus a cursor row, so 13 rows are padding.
    let before = physical_rows(&s);
    let cursor_before = s.cursor().position.row;
    assert_eq!(cursor_before, 10);

    // Shrink to 6 (18 rows): 13 padding rows are absorbed and 5 content
    // rows move into scrollback (the mixed case, not a pure absorb or pure
    // push).
    s.resize(80, 6);
    assert_eq!(s.scrollback_len(), 5);
    assert!(s.check_invariants().is_ok());

    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        s.scrollback_len(),
        0,
        "the grow must re-enter every pushed row"
    );
    assert_eq!(
        s.cursor().position.row,
        cursor_before,
        "the cursor must ride its content back to the original row"
    );
    assert_eq!(
        physical_rows(&s),
        before,
        "the shrink/grow round trip must restore every row byte-for-byte"
    );
}

#[test]
fn width_roundtrip_preserves_wrap_boundary_spaces() {
    let mut s = State::new();
    // 79 'a' fill the row through column 78; the space lands in the last
    // column and is soft-wrapped content, not padding.
    prints(&mut s, &"a".repeat(79));
    prints(&mut s, " tail");
    let before = logical_text(&s);
    assert!(
        before
            .lines()
            .next()
            .is_some_and(|l| l == format!("{} tail", "a".repeat(79))),
        "fixture must produce one logical line with a boundary space: {before:?}"
    );

    s.resize(40, 24);
    assert!(s.check_invariants().is_ok());
    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    let after = logical_text(&s);
    assert_eq!(
        after.lines().next(),
        Some(format!("{} tail", "a".repeat(79)).as_str()),
        "narrow/widen must not merge the words across the wrap boundary: {after:?}"
    );
}

#[test]
fn width_roundtrip_preserves_ls_and_markdown_output() {
    let mut s = State::new();
    let mut lines = Vec::new();
    // `ls`-style padded columns, a long soft-wrapped filename, and Markdown
    // prose with inline code, links, and blank lines.
    lines.push("total 48".to_string());
    lines.push("drwxr-xr-x  4 user user 4096 Oct 23 12:00 .".to_string());
    lines.push("drwxr-xr-x 12 user user 4096 Oct 23 11:59 ..".to_string());
    lines.push(format!(
        "-rw-r--r--  1 user user 8192 Oct 23 12:00 {}.md",
        "long-name-".repeat(12)
    ));
    lines.push("# Bitty terminal".to_string());
    lines.push(String::new());
    lines.push("Resizing a panel must keep **every logical line** intact, including `code`, [links](https://example.com/a/very/long/path/that/keeps/going), and prose that soft-wraps across several physical rows without merging words together.".to_string());
    lines.push(String::new());
    lines.push("- first item".to_string());
    lines.push("- second item".to_string());
    for line in &lines {
        prints(&mut s, line);
        newline(&mut s);
    }
    assert!(s.check_invariants().is_ok());

    let before = logical_text(&s);
    let before_content = before.trim_end_matches('\n');
    for width in [33usize, 120, 41, 80] {
        s.resize(width, 24);
        assert!(s.check_invariants().is_ok(), "invariants at width {width}");
        assert_no_orphan_wides(&s);
        let text = logical_text(&s);
        assert_eq!(
            text.trim_end_matches('\n'),
            before_content,
            "width {width} must preserve the exact logical lines"
        );
    }
}

#[test]
fn wide_and_combining_glyphs_survive_odd_width_roundtrip() {
    let mut s = State::new();
    prints(&mut s, &"中".repeat(40));
    prints(&mut s, "́");
    assert!(s.check_invariants().is_ok());

    for width in [41usize, 23, 1, 80] {
        s.resize(width, 24);
        assert!(s.check_invariants().is_ok(), "invariants at width {width}");
        assert_no_orphan_wides(&s);
    }
    let text = logical_text(&s);
    let expected = format!("{}́", "中".repeat(40));
    assert_eq!(
        text.lines().next(),
        Some(expected.as_str()),
        "wide glyphs and their combining marks must survive exactly: {text:?}"
    );
}

#[test]
fn image_placement_follows_height_and_clears_on_width_reflow() {
    fn place(state: &mut State, image: u32, placement: u32, cols: u16, rows: u16) {
        state.apply(&TerminalAction::KittyGraphics {
            format_f: 0,
            width_s: None,
            height_v: None,
            action_a: Some('p'),
            cols_c: cols,
            rows_r: rows,
            cursor_movement_c: 0,
            payload: Box::default(),
            control: KittyControlKeys {
                image_id: image,
                placement_id: placement,
                ..Default::default()
            },
        });
    }
    fn prototype(state: &mut State, image: u32) {
        state.apply(&TerminalAction::KittyGraphics {
            format_f: 0,
            width_s: None,
            height_v: None,
            action_a: Some('p'),
            cols_c: 2,
            rows_r: 1,
            cursor_movement_c: 1,
            payload: Box::default(),
            control: KittyControlKeys {
                image_id: image,
                placement_id: 0,
                unicode_placement: 1,
                ..Default::default()
            },
        });
    }

    let mut s = State::new();
    s.apply(&TerminalAction::CursorPosition {
        row: Row(20),
        col: Col(1),
    });
    place(&mut s, 7, 0, 3, 2);
    // Cursor advance (C=0) lands on row 21; park it on the last row so the
    // viewport is bottom-full and the shrink is bottom-anchored.
    prototype(&mut s, 9);
    s.apply(&TerminalAction::CursorPosition {
        row: Row(24),
        col: Col(1),
    });
    assert_eq!(s.kitty_placements().get(7, 0).unwrap().anchor_row, 19);
    assert_eq!(s.cursor().position.row, 23);

    s.resize(80, 12);
    assert!(s.check_invariants().is_ok());
    assert_eq!(s.kitty_placements().get(7, 0).unwrap().anchor_row, 7);
    assert!(
        s.kitty_placements().get(9, 0).unwrap().virtual_proto,
        "virtual prototypes have no anchor and must survive"
    );

    s.resize(80, 24);
    assert_eq!(s.kitty_placements().get(7, 0).unwrap().anchor_row, 19);

    // A width reflow rebuilds every row: anchored placements are cleared
    // safely, while the virtual prototype (text-rendered) is retained.
    s.resize(40, 24);
    assert!(s.check_invariants().is_ok());
    assert!(s.kitty_placements().get(7, 0).is_none());
    assert!(s.kitty_placements().get(9, 0).is_some());
}

/// #2543 cursor-detach edge: a shrink larger than the absorbable padding
/// pushes the cursor's own row into scrollback. The content is preserved, but
/// the cursor clamps to row 0 because its row is no longer in the viewport
/// (keeping it attached would require dropping newer rows).
#[test]
fn shrink_past_cursor_row_keeps_content_and_clamps_cursor() {
    let mut s = State::new();
    let mut lines = Vec::new();
    for i in 0..12 {
        let line = format!("body-{i:02}");
        prints(&mut s, &line);
        newline(&mut s);
        lines.push(line);
    }
    // Park the cursor on row 4 with non-blank rows 5..11 below it.
    s.apply(&TerminalAction::CursorPosition {
        row: Row(5),
        col: Col(1),
    });
    assert_eq!(s.cursor().position.row, 4);
    assert_each_once_in_order(&addressable_text(&s), &lines);

    // used = 12 rows, so 24 -> 5 absorbs 12 blank rows and pushes 7 content
    // rows; the cursor row is inside that pushed prefix.
    s.resize(80, 5);
    assert!(s.check_invariants().is_ok());
    assert_eq!(s.scrollback_len(), 7);
    assert_eq!(
        s.cursor().position.row,
        0,
        "a pushed cursor row clamps to the viewport top"
    );
    let after = addressable_text(&s);
    assert_each_once_in_order(&after, &lines);
    assert!(
        after.contains("body-11"),
        "newest content must stay in the viewport: {after:?}"
    );
}

/// #2543 "only when" direction: a shrink that leaves enough rows for the
/// cursor row plus every used row below it keeps the cursor attached, so it
/// shifts with its materialized row instead of clamping to row 0.
#[test]
fn shrink_that_keeps_cursor_row_attaches_cursor_to_shifted_content() {
    let mut s = State::new();
    let mut lines = Vec::new();
    for i in 0..12 {
        let line = format!("body-{i:02}");
        prints(&mut s, &line);
        newline(&mut s);
        lines.push(line);
    }
    // Cursor on row 4; used = 12, so attachment needs 12 - 4 = 8 rows.
    s.apply(&TerminalAction::CursorPosition {
        row: Row(5),
        col: Col(1),
    });
    assert_eq!(s.cursor().position.row, 4);

    // 10 >= 8 rows: absorb the 12 padding rows, push 2, and move the cursor
    // with its row (4 -> 2) instead of clamping it.
    s.resize(80, 10);
    assert!(s.check_invariants().is_ok());
    assert_eq!(s.scrollback_len(), 2);
    assert_eq!(
        s.cursor().position.row,
        2,
        "an attached cursor shifts with its content"
    );
    let after = addressable_text(&s);
    assert_each_once_in_order(&after, &lines);
    let snap = s.snapshot();
    let start = 2 * snap.width;
    assert_eq!(
        row_text(&snap.cells[start..start + snap.width]),
        "body-04",
        "the cursor's row must still hold its own content"
    );
}

/// #2543 bounded-memory tradeoff: a 1-column reflow repacks every printable
/// cell into its own physical row. The scrollback cap then prunes the oldest
/// rows exactly like normal scroll-under pruning: bounded memory, no panic,
/// no orphan wide halves, and the retained suffix reflows back losslessly.
#[test]
fn one_column_reflow_enforces_scrollback_cap_without_corruption() {
    const CAP: usize = 64;
    let mut s = State::with_scrollback_lines(CAP);
    let mut lines = Vec::new();
    for i in 0..500 {
        let line = format!("line-{i:03}-{}", "x".repeat(40));
        prints(&mut s, &line);
        newline(&mut s);
        lines.push(line);
    }
    assert_eq!(s.scrollback_len(), CAP);
    assert!(s.check_invariants().is_ok());

    // 1-column pass: ~50 cells per line become ~50 rows, far beyond CAP.
    s.resize(1, 24);
    assert_eq!((s.width(), s.height()), (1, 24));
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        s.scrollback_len(),
        CAP,
        "the line cap bounds the 1-column repack"
    );
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 1, "1-column rows are exactly one cell");
        assert!(!line.cells[0].spacer, "no orphan wide spacer at width 1");
        assert!(line.cells[0].width <= 1, "wide leads narrow at width 1");
    }

    // Widening back must reconstruct the retained suffix without corruption:
    // the newest full line is still present exactly once and wide invariants
    // hold across the rebuilt rows.
    s.resize(80, 24);
    assert_eq!((s.width(), s.height()), (80, 24));
    assert_eq!(s.snapshot().cells.len(), 80 * 24);
    assert!(s.check_invariants().is_ok());
    assert_no_orphan_wides(&s);
    let restored = addressable_text(&s);
    let newest = lines.last().expect("fixture line");
    assert_each_once_in_order(&restored, std::slice::from_ref(newest));
}

#[test]
fn alt_screen_resize_stays_bounded_and_keeps_primary_history() {
    let mut s = State::new();
    prints(&mut s, "primary-output");
    newline(&mut s);
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    for i in 0..40 {
        prints(&mut s, &format!("tui-{i:02}"));
        newline(&mut s);
    }
    // Repeated aggressive shrinks/grows: bounded truncate/pad, no reflow of
    // application-owned alt content, no panic.
    for (cols, rows) in [(20, 1), (20, 5), (7, 2), (20, 5)] {
        s.resize(cols, rows);
        assert!(s.check_invariants().is_ok());
    }
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert!(!s.alt_screen_active());
    assert!(s.check_invariants().is_ok());
    assert!(
        logical_text(&s).contains("primary-output"),
        "primary content must survive alt-screen resizes: {:?}",
        logical_text(&s)
    );
}

#[test]
fn hyperlink_spans_survive_width_reflow() {
    let mut s = State::new();
    s.apply(&TerminalAction::OscHyperlink {
        link: Some(Hyperlink {
            id: None,
            uri: BoundedString::new("https://example.com/a/long/linked/path"),
        }),
    });
    let linked = "linked text ".repeat(10);
    prints(&mut s, &linked);
    s.apply(&TerminalAction::OscHyperlink { link: None });
    prints(&mut s, "plain");
    let id = s.current_hyperlink().unwrap_or_else(|| {
        // The link is closed; resolve the id from the first linked cell.
        let snap = s.snapshot();
        snap.cells[0].hyperlink.expect("first cell linked")
    });
    let linked_cells = |state: &State| -> usize {
        let mut count = 0;
        for line in state.scrollback() {
            count += line
                .cells
                .iter()
                .filter(|cell| cell.hyperlink == Some(id))
                .count();
        }
        let snap = state.snapshot();
        count
            + snap
                .cells
                .iter()
                .filter(|cell| cell.hyperlink == Some(id))
                .count()
    };
    assert_eq!(linked_cells(&s), linked.chars().count());
    assert_eq!(
        s.hyperlink_entry(id),
        Some((None, "https://example.com/a/long/linked/path"))
    );

    s.resize(33, 24);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        linked_cells(&s),
        linked.chars().count(),
        "hyperlink ids must travel with every rewrapped cell"
    );
    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    assert_eq!(linked_cells(&s), linked.chars().count());
    assert_eq!(
        s.hyperlink_entry(id),
        Some((None, "https://example.com/a/long/linked/path"))
    );
}

/// #2529 review blocker: a wide glyph that straddles the right margin gets a
/// synthetic padding blank during rewrap. That blank must never become a
/// content space on the next unwrap, or repeated width cycles insert spaces
/// into CJK/emoji text.
#[test]
fn wide_reflow_is_exact_for_cjk_repeated_cycles() {
    let paragraph = "这是一段很长的中文段落，用来检查窗口宽度变化时是否会在字符之间插入多余的空格，从而破坏可复制文本。";
    let mut s = State::new();
    prints(&mut s, paragraph);
    newline(&mut s);
    let before = logical_text(&s);
    assert_eq!(before.lines().next(), Some(paragraph));

    for _ in 0..5 {
        s.resize(37, 24);
        assert!(s.check_invariants().is_ok());
        s.resize(80, 24);
        assert!(s.check_invariants().is_ok());
    }
    assert_eq!(
        logical_text(&s),
        before,
        "37<->80 cycles must not insert spaces between wide glyphs"
    );
}

/// One-step forms: the synthetic margin pad disappears, while a real space
/// printed in the last column survives exactly.
#[test]
fn wide_margin_padding_is_not_content_but_real_spaces_are() {
    // No printed space: "AAAA中" wraps with a synthetic pad after AAAA.
    for width in [4usize, 5, 6] {
        let mut s = State::new();
        s.resize(width, 24);
        prints(&mut s, "AAAA中");
        newline(&mut s);
        s.resize(80, 24);
        assert_eq!(
            logical_text(&s).lines().next(),
            Some("AAAA中"),
            "synthetic wide-margin padding at width {width} must not materialize"
        );
    }

    // A real space in the last column before the wide glyph must survive.
    for width in [4usize, 5, 6] {
        let mut s = State::new();
        s.resize(width, 24);
        prints(&mut s, "AAAA 中");
        newline(&mut s);
        s.resize(80, 24);
        assert_eq!(
            logical_text(&s).lines().next(),
            Some("AAAA 中"),
            "a printed space at width {width} must remain content"
        );
    }
}

/// Emoji (width 2), mixed ASCII, and meaningful runs of real spaces must be
/// byte-exact across boundary widths, repeated cycles, and a 1-column pass.
#[test]
fn wide_emoji_and_mixed_spaces_survive_repeated_cycles() {
    let text = "mixed 😀 text  中文字符 with  real  spaces  and emoji 😀😀 end";
    let mut s = State::new();
    prints(&mut s, text);
    newline(&mut s);
    let before = logical_text(&s);
    assert_eq!(before.lines().next(), Some(text));

    for width in [37usize, 39, 40, 41, 5, 80, 1, 80, 37, 80] {
        s.resize(width, 24);
        assert!(s.check_invariants().is_ok(), "invariants at width {width}");
        assert_eq!(
            logical_text(&s),
            before,
            "width {width} must preserve the exact logical text"
        );
    }
}

/// Regression test for #3103 (right pane): an inline primary-screen TUI status
/// bar rendered via DECSET 2026 synchronized updates, `ED 0` (`\x1b[J`), and a
/// trailing carriage return (`\r`) must never soft-wrap into stranded rows or
/// corrupt its column-0 prefix (`"ediesc to cancel..."`) across continuous
/// shrinking/growing border-drag resizes and in-flight frames formatted for
/// the previous width.
#[test]
fn inline_tui_status_bar_drag_resize_does_not_staircase_or_corrupt_prefix() {
    fn sync_begin(state: &mut State) {
        state.apply(&TerminalAction::SetMode {
            mode: Mode::SynchronizedUpdate,
            enabled: true,
        });
        state.apply(&TerminalAction::CursorVisibility { visible: false });
    }
    fn sync_end(state: &mut State) {
        state.apply(&TerminalAction::SetMode {
            mode: Mode::SynchronizedUpdate,
            enabled: false,
        });
    }
    fn render_status_bar(state: &mut State, cols: usize, send_leading_cr: bool) {
        let prefix = " esc to cancel ";
        let suffix = "accept-edits · Gemini 4 Argon · medium · AI: Out of credits";
        let pad = cols.saturating_sub(prefix.chars().count() + suffix.chars().count());
        let full = format!("{prefix}{}{suffix}", " ".repeat(pad));
        let line: String = full.chars().take(cols).collect();
        sync_begin(state);
        if send_leading_cr {
            control(state, 0x0D);
        }
        state.apply(&TerminalAction::EraseInDisplay {
            mode: bitty_vt::EraseDisplayMode::Below,
        });
        prints(state, &line);
        control(state, 0x0D);
        sync_end(state);
    }

    let mut s = State::new();
    s.resize(83, 39);
    // Park cursor on the bottom row (row 38) as in the right pane of #3103.
    s.apply(&TerminalAction::CursorPosition {
        row: Row(39),
        col: Col(1),
    });
    render_status_bar(&mut s, 83, true);
    assert!(s.check_invariants().is_ok());

    // Simulate continuous mouse border-drag shrinking and growing (83 -> 81 ->
    // 76 -> 73 -> 70 -> 74 -> 80 -> 83), including in-flight PTY frames
    // formatted for the previous width arriving after the grid has already
    // narrowed, and `ED 0` redraws without a leading `\r` (since the TUI's
    // internal cursor already sits at x = 0 after the previous trailing `\r`).
    let steps = [
        (81usize, 83usize),
        (76, 81),
        (73, 76),
        (70, 73),
        (74, 70),
        (80, 74),
        (83, 80),
    ];
    for (new_w, inflight_w) in steps {
        s.resize(new_w, 39);
        assert!(s.check_invariants().is_ok());
        // In-flight frame formatted for `inflight_w` arrives before SIGWINCH redraw:
        render_status_bar(&mut s, inflight_w, false);
        assert!(s.check_invariants().is_ok());
        // SIGWINCH redraw for `new_w`:
        render_status_bar(&mut s, new_w, false);
        assert!(s.check_invariants().is_ok());

        let snap = s.snapshot();
        for r in 0..38 {
            let row = row_text(&snap.cells[r * snap.width..(r + 1) * snap.width]);
            assert!(
                row.is_empty(),
                "row {r} above status bar must stay empty at width {new_w}, got {row:?}"
            );
        }
        let bottom = row_text(&snap.cells[38 * snap.width..39 * snap.width]);
        assert!(
            bottom.starts_with(" esc to cancel "),
            "status bar prefix corrupted at width {new_w}: {bottom:?}"
        );
        assert_eq!(
            bottom.matches("esc to cancel").count(),
            1,
            "status bar must appear once on the bottom row at width {new_w}: {bottom:?}"
        );
    }
    assert_eq!(
        s.scrollback_len(),
        0,
        "no status bar rows may leak into scrollback"
    );
}

/// Number of retained cells whose glyph is `glyph`, across scrollback and the
/// live grid (data-loss checks count glyphs, not rows).
fn count_glyph(state: &State, glyph: char) -> usize {
    physical_rows(state)
        .iter()
        .flat_map(|(cells, _)| cells.iter())
        .filter(|cell| !cell.spacer && cell.glyph == glyph)
        .count()
}

fn begin_sync_frame(state: &mut State) {
    state.apply(&TerminalAction::SetMode {
        mode: Mode::SynchronizedUpdate,
        enabled: true,
    });
    state.apply(&TerminalAction::CursorVisibility { visible: false });
}

fn end_sync_frame(state: &mut State) {
    state.apply(&TerminalAction::SetMode {
        mode: Mode::SynchronizedUpdate,
        enabled: false,
    });
}

fn cursor_up(state: &mut State, n: u16) {
    state.apply(&TerminalAction::CursorMove {
        dir: bitty_vt::Direction::Up,
        n: bitty_vt::Count(n),
    });
}

/// Regression test for #3103 (review probe 3): an inline TUI widget whose
/// recorded top row is a soft-wrapped *continuation* row belongs to a logical
/// line that began above the widget. That logical line carries committed
/// history, so a width change must reflow it instead of truncating it 1-to-1.
#[test]
fn inline_tui_widget_top_on_continuation_row_keeps_the_logical_above() {
    let mut s = State::new();
    s.resize(20, 6);
    // One over-width logical line: 30 `H` occupy rows 0-1 with no linefeed.
    prints(&mut s, &"H".repeat(30));
    newline(&mut s);
    // Step onto the continuation row (row 1), then open a synchronized frame
    // there: the widget top now sits inside the logical printed above it.
    cursor_up(&mut s, 1);
    begin_sync_frame(&mut s);
    assert!(s.check_invariants().is_ok());

    s.resize(10, 6);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'H'),
        30,
        "the logical line that began above the widget top must reflow, not truncate: {:?}",
        logical_text(&s)
    );
    assert!(
        logical_text(&s).contains(&"H".repeat(30)),
        "the over-width line above the widget top must stay one logical line: {:?}",
        logical_text(&s)
    );
}

/// Regression test for #3103 (review probe 5): ordinary unsynchronized `CR`
/// and cursor-up redraws (progress bars, `printf '\r'`, readline repaint) must
/// never arm the inline 1-to-1 resize path, which would truncate soft-wrapped
/// output that has not scrolled into history yet.
#[test]
fn unsynchronized_carriage_return_and_cursor_up_never_arm_inline_truncation() {
    // A plain `\r` after a soft-wrapped run with no linefeed.
    let mut s = State::new();
    prints(&mut s, &"w".repeat(35));
    control(&mut s, 0x0D);
    s.resize(10, 6);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'w'),
        35,
        "a plain CR redraw must keep normal wrapping and reflow: {:?}",
        logical_text(&s)
    );

    // A plain cursor-up (no DECSET 2026) on the same shape.
    let mut s = State::new();
    prints(&mut s, &"u".repeat(35));
    cursor_up(&mut s, 1);
    s.resize(10, 6);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'u'),
        35,
        "a plain cursor-up redraw must keep normal wrapping and reflow: {:?}",
        logical_text(&s)
    );
}

/// Regression test for #3103 (review probe 6): an inline TUI that emits
/// scrollback history inside its `?2026h` frame (the `tea.Println` shape) must
/// keep every glyph of an over-width history line at a steady width — those
/// glyphs are committed history and no later redraw can recover them.
#[test]
fn synchronized_frame_history_overflow_keeps_every_glyph() {
    let mut s = State::new();
    s.resize(20, 6);
    begin_sync_frame(&mut s);
    for i in 0..6 {
        prints(&mut s, &format!("L{i}-"));
        newline(&mut s);
    }
    // Over-width history line in the same frame: 30 `q` at width 20.
    prints(&mut s, &"q".repeat(30));
    end_sync_frame(&mut s);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'q'),
        30,
        "an in-frame history line must soft-wrap and keep every glyph: {:?}",
        logical_text(&s)
    );
    assert!(
        s.scrollback_len() > 0,
        "the in-frame history lines must reach scrollback"
    );
}

/// Regression test for #3103 (round-2 review probe 10): `ED 1` only blanks the
/// rows *above* the cursor, so rows between the cursor row and a recorded
/// widget top still hold text. Lowering the 1-to-1 boundary onto the cursor
/// row would pull those unerased rows into the truncating region and lose
/// glyphs on the next width change; the record must be kept as-is.
///
/// Shape: 30 `h` on rows 0-1, 30 `j` on rows 2-3, `DECSC` on row 1, a `?2026h`
/// frame opened on row 3 (widget top = 3), `DECRC` back to row 1, `ED 1`, then
/// a narrowing resize. The `j` logical line sits entirely below the cursor, so
/// it is never erased and must reflow intact.
#[test]
fn ed1_above_keeps_unerased_rows_between_cursor_and_widget_top() {
    let mut s = State::new();
    s.resize(20, 6);
    // 30 `h` occupy rows 0-1 (soft wrap, no linefeed between them).
    prints(&mut s, &"h".repeat(30));
    newline(&mut s);
    // 30 `j` occupy rows 2-3: committed history that sits above the widget
    // top that is about to be recorded, but below the cursor after DECRC.
    prints(&mut s, &"j".repeat(30));
    // DECSC on row 1 (inside the `h` logical line).
    s.apply(&TerminalAction::CursorPosition {
        row: Row(2),
        col: Col(11),
    });
    s.apply(&TerminalAction::CursorSave);
    // A `?2026h` frame opens at row 3: the recorded widget top is row 3.
    s.apply(&TerminalAction::CursorPosition {
        row: Row(4),
        col: Col(11),
    });
    begin_sync_frame(&mut s);
    // DECRC returns the cursor to row 1 without re-deriving the widget top.
    s.apply(&TerminalAction::CursorRestore);
    // ED 1 blanks only rows above (and left of) the cursor: rows 2-3 keep
    // their `j` glyphs.
    s.apply(&TerminalAction::EraseInDisplay {
        mode: bitty_vt::EraseDisplayMode::Above,
    });
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'j'),
        30,
        "ED 1 above the cursor must not erase the rows below it: {:?}",
        logical_text(&s)
    );

    s.resize(10, 6);
    assert!(s.check_invariants().is_ok());
    assert_eq!(
        count_glyph(&s, 'j'),
        30,
        "rows between the cursor row and the widget top survive ED 1, so they \
         must reflow instead of truncating 1-to-1: {:?}",
        logical_text(&s)
    );
}

/// Regression test for #3103 (left pane): a multi-line inline prompt widget
/// (`────` / `> ` / `────` / `? for shortcuts`) rendered below committed
/// Markdown history lines must not leave duplicate `────────` borders above
/// `> ` when shrinking/growing the pane width, while the committed Markdown
/// history above the inline widget still reflows losslessly.
#[test]
fn inline_tui_prompt_widget_drag_resize_preserves_history_reflow_without_duplicate_borders() {
    fn render_prompt_widget(state: &mut State, cols: usize, initial: bool) {
        state.apply(&TerminalAction::SetMode {
            mode: Mode::SynchronizedUpdate,
            enabled: true,
        });
        state.apply(&TerminalAction::CursorVisibility { visible: false });
        if !initial {
            // Cursor is parked on `> ` (1 row below top border): `\r\x1b[1A\x1b[J`.
            control(state, 0x0D);
            state.apply(&TerminalAction::CursorMove {
                dir: bitty_vt::Direction::Up,
                n: bitty_vt::Count(1),
            });
        }
        state.apply(&TerminalAction::EraseInDisplay {
            mode: bitty_vt::EraseDisplayMode::Below,
        });
        prints(state, &"─".repeat(cols));
        newline(state);
        prints(state, "> ");
        newline(state);
        prints(state, &"─".repeat(cols));
        newline(state);
        prints(state, "? for shortcuts");
        // Park cursor on `> ` (2 rows up from `? for shortcuts`, column 3).
        state.apply(&TerminalAction::CursorMove {
            dir: bitty_vt::Direction::Up,
            n: bitty_vt::Count(2),
        });
        state.apply(&TerminalAction::CursorPosition {
            row: Row::SENTINEL,
            col: Col(3),
        });
        state.apply(&TerminalAction::CursorVisibility { visible: true });
        state.apply(&TerminalAction::SetMode {
            mode: Mode::SynchronizedUpdate,
            enabled: false,
        });
    }

    let mut s = State::new();
    s.resize(83, 24);
    let history_paragraph = "Resizing a split panel with an active inline TUI prompt must reflow committed Markdown prose above the prompt while keeping the inline prompt borders 1-to-1.";
    for i in 0..22 {
        prints(&mut s, &format!("history-{i:02}: {history_paragraph}"));
        newline(&mut s);
    }
    render_prompt_widget(&mut s, 83, true);
    assert!(s.check_invariants().is_ok());

    for &w in &[78usize, 72, 64, 58, 70, 83] {
        s.resize(w, 24);
        assert!(s.check_invariants().is_ok());
        render_prompt_widget(&mut s, w, false);
        assert!(s.check_invariants().is_ok());

        let snap = s.snapshot();
        let border = "─".repeat(w);
        let border_rows: Vec<usize> = (0..snap.height)
            .filter(|&r| {
                let text = row_text(&snap.cells[r * snap.width..(r + 1) * snap.width]);
                text.chars().all(|c| c == '─') && !text.is_empty()
            })
            .collect();
        assert_eq!(
            border_rows,
            vec![20, 22],
            "inline widget must have exactly 2 border rows at width {w}, got rows {border_rows:?}"
        );
        assert_eq!(
            row_text(&snap.cells[20 * snap.width..21 * snap.width]),
            border
        );
        assert_eq!(row_text(&snap.cells[21 * snap.width..22 * snap.width]), ">");
        assert_eq!(
            row_text(&snap.cells[22 * snap.width..23 * snap.width]),
            border
        );
        assert_eq!(
            row_text(&snap.cells[23 * snap.width..24 * snap.width]),
            "? for shortcuts"
        );
    }
    let full_logical = logical_text(&s);
    assert!(
        full_logical.contains(&format!("history-21: {history_paragraph}")),
        "committed Markdown history above the inline widget must round-trip losslessly: {full_logical:?}"
    );
}
