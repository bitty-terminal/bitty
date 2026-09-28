//! Kitty Unicode placeholder end-to-end tests (CTX-0821, issue #1400).
//!
//! Style mirrors `kitty_images_present.rs`: headless `Runtime` seams prove
//! that programs emitting `U+10EEEE` runs get placement-sized,
//! deterministically-managed grid cells instead of tofu.
//!
//! Covered here (state + metadata + semantics slice):
//! - placeholder runs decode to image/tile identities through PTY bytes
//!   (SGR foreground colors + combining diacritics);
//! - run geometry resolves against a virtual span via
//!   `bitty_rich::kitty_unicode::unicode_run_rect` (placement-sized cells);
//! - `Runtime::kitty_unicode_delete` clears named runs (delete semantics);
//! - resize/reflow and erase keep or drop runs deterministically.
//!
//! Follow-up work (recorded in the PR body, not here): `U=1` virtual
//! placement registration in the parser/runtime (`KittyGraphics` carries
//! no `U`/`i`/`p` yet), render-time compositing of run tiles, and
//! cursor-movement (`C=`) interplay.

use bitty_runtime::{Runtime, RuntimeConfig};

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

fn fg42(cell: &bitty_term_state::Cell) -> bool {
    cell.style.foreground == Some(bitty_vt::Color::Indexed(42))
}

#[test]
fn placeholder_run_decodes_through_pty_bytes() {
    // SGR 38;5;42 names image 42; U+10EEEE + row/col diacritics name
    // tiles (0,0) and (0,1). The trailing SGR 39 reset must not leak
    // into later text.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(
        "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39mAB".as_bytes(),
    );
    let snap = rt.snapshot();
    assert_eq!(snap.cells[0].glyph, '\u{10EEEE}');
    assert_eq!(snap.cells[1].glyph, '\u{10EEEE}');
    assert!(fg42(&snap.cells[0]) && fg42(&snap.cells[1]));
    assert_eq!(snap.cells[2].glyph, 'A');
    assert_eq!(snap.cells[2].style.foreground, None);
    let (cells, key) = rt
        .state()
        .kitty_unicode_run_at(0, 0)
        .expect("run must decode through PTY bytes");
    assert_eq!(key, (42, None));
    assert_eq!(cells.len(), 2);
    assert_eq!((cells[0].id.row, cells[0].id.col), (0, 0));
    assert_eq!((cells[1].id.row, cells[1].id.col), (0, 1));
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 1);
}

#[test]
fn placeholder_run_sizes_against_virtual_span() {
    // The 2-cell run covers tile columns 0..2 of a 4x2 virtual span, so
    // the resolved rect is 2 cells wide at the run's grid origin.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(
        "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39m".as_bytes(),
    );
    let (cells, _) = rt.state().kitty_unicode_run_at(0, 0).expect("run");
    let run = bitty_term_state::KittyUnicodeRun {
        image_id: 42,
        placement_id: None,
        row: 0,
        col: 0,
        grid_row: 0,
        grid_col: 0,
        width: cells.len(),
    };
    let metrics = bitty_rich::CellMetrics {
        width: 9,
        height: 19,
    };
    let span = bitty_rich::KittyUnicodeVirtual {
        image_id: 42,
        placement_id: None,
        cols: 4,
        rows: 2,
        image_width: 36,
        image_height: 38,
    };
    let rect = bitty_rich::unicode_run_rect(&run, &cells, &span, metrics, 0, 0).expect("rect");
    assert_eq!((rect.grid_row, rect.grid_col), (0, 0));
    assert_eq!((rect.cols, rect.rows), (2, 1));
    assert_eq!(
        rect.rect,
        bitty_rich::RectPx::new(0, 0, 2 * 9, 19),
        "placement-sized cells: 2 run cells at 9x19"
    );
    // A mismatched span (other image) resolves to no rect.
    let other = bitty_rich::KittyUnicodeVirtual {
        image_id: 43,
        ..span
    };
    assert!(bitty_rich::unicode_run_rect(&run, &cells, &other, metrics, 0, 0).is_none());
    let _ = RuntimeConfig::default();
}

#[test]
fn placeholder_delete_clears_named_runs_only() {
    let mut rt = make_runtime();
    // Image-7 run then image-8 run on the same row.
    rt.handle_pty_bytes(
        "\x1b[38;5;7m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39m".as_bytes(),
    );
    rt.handle_pty_bytes("\x1b[38;5;8m\u{10EEEE}\u{0305}\u{0305}\x1b[39m".as_bytes());
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 2);
    assert_eq!(
        rt.kitty_unicode_delete(99, None),
        0,
        "unknown id clears nothing"
    );
    assert_eq!(rt.kitty_unicode_delete(7, None), 2);
    let snap = rt.snapshot();
    assert!(snap.cells[0].is_blank());
    assert!(snap.cells[1].is_blank());
    assert_eq!(snap.cells[2].glyph, '\u{10EEEE}');
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 1);
    assert!(rt.tick().is_some(), "delete forces a present");
}

#[test]
fn placeholder_resize_reflow_keeps_cells_deterministic() {
    // A full-width line starting with a 2-cell run rewraps on narrowing;
    // both placeholder cells survive (grid or scrollback), invariants hold.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(
        "\x1b[38;5;7m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39m".as_bytes(),
    );
    for _ in 0..78 {
        rt.handle_pty_bytes(b"x");
    }
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 1);
    // Drive the same reflow through the runtime's terminal registry path
    // is out of scope for this slice (workspaces own that file); assert
    // the state-level contract directly on a fresh State instead.
    let mut st = bitty_term_state::State::new();
    st.apply(&bitty_vt::TerminalAction::SetAttributes {
        attrs: bitty_vt::AttributeDiff {
            changes: vec![bitty_vt::AttributeChange::Foreground(
                bitty_vt::Color::Indexed(7),
            )]
            .into_boxed_slice(),
        },
    });
    for c in "\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}".chars() {
        st.apply(&bitty_vt::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(c),
        ));
    }
    for c in "x".repeat(78).chars() {
        st.apply(&bitty_vt::TerminalAction::Print(
            bitty_vt::GraphemeCell::from(c),
        ));
    }
    st.resize(40, 24);
    assert!(st.check_invariants().is_ok());
    let grid_cells = st
        .snapshot()
        .cells
        .iter()
        .filter(|c| c.glyph == '\u{10EEEE}')
        .count();
    let sb_cells = st
        .scrollback()
        .flat_map(|line| line.cells.iter())
        .filter(|c| c.glyph == '\u{10EEEE}')
        .count();
    assert_eq!(
        grid_cells + sb_cells,
        2,
        "both placeholder cells survive reflow"
    );
}

#[test]
fn placeholder_erase_drops_runs() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(
        "\x1b[38;5;7m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39m".as_bytes(),
    );
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 1);
    // EL 2 (whole line) erases the run.
    rt.handle_pty_bytes(b"\x1b[2K");
    assert!(rt.kitty_unicode_runs_on_row(0).is_empty());
    assert!(rt.state().check_invariants().is_ok());
}
