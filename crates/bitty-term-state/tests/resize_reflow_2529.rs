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
