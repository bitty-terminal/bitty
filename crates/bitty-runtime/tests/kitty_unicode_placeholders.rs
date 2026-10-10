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
//! Follow-up work (recorded in the PR body, not here): render-time
//! compositing of run tiles and cursor-movement (`C=`) interplay.
//!
//! S4 (#1849, CTX-1101) covers the `U=1` bookkeeping below: bodiless
//! `a=p,U=1` and combined `a=T,U=1` register origin-tagged prototypes in
//! the rich registry (bounded, quota-shared with blit placements),
//! runs resolve against them without emitting blits (double-paint rule),
//! scroll follows the `scrollback_base` delta, identity deletes and full
//! clears drop both layers, and status queries answer held/OK.

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

// ---------------------------------------------------------------------------
// S4 (#1849, CTX-1101): `U=1` virtual-prototype bookkeeping.
// ---------------------------------------------------------------------------

/// 2x2 opaque red RGBA payload (`f=32`).
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

fn metrics_9x19() -> bitty_rich::CellMetrics {
    bitty_rich::CellMetrics {
        width: 9,
        height: 19,
    }
}

fn print_run_42(rt: &mut Runtime) {
    rt.handle_pty_bytes(
        "\x1b[38;5;42m\u{10EEEE}\u{0305}\u{0305}\u{10EEEE}\u{0305}\u{030D}\x1b[39m".as_bytes(),
    );
}

#[test]
fn virtual_bodiless_registers_and_resolves_without_blits() {
    // Bodiless `a=p,U=1,i=42,c=4,r=2`: terminal truth and the rich
    // registry record the prototype, no blit placement exists, and the
    // printed run resolves to a rect (double-paint rule: a concurrent
    // real blit still paints exactly one blit).
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=11,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 1);
    let cursor_before = rt.state().cursor().position;
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    assert_eq!(rt.kitty_virtual_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1, "no blit for the prototype");
    assert!(
        rt.state().kitty_placements().get(42, 0).is_some(),
        "terminal truth records the prototype"
    );
    assert_eq!(
        rt.state().cursor().position,
        cursor_before,
        "virtual placement moves no cursor"
    );
    // The display above advanced the cursor: home it so the run prints
    // on row 0.
    rt.handle_pty_bytes(b"\x1b[H");
    print_run_42(&mut rt);
    let rects = rt.kitty_unicode_run_rects_on_row(0, metrics_9x19());
    assert_eq!(rects.len(), 1, "run resolves against the prototype");
    assert_eq!((rects[0].grid_row, rects[0].grid_col), (0, 0));
    assert_eq!((rects[0].cols, rects[0].rows), (2, 1));
    assert_eq!(rects[0].rect, bitty_rich::RectPx::new(0, 0, 2 * 9, 19));
    assert!(rt.tick().is_some());
    assert_eq!(
        rt.kitty_last_frame_images(),
        1,
        "zero blits for the prototype alongside one real blit"
    );
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_combined_transmit_registers_without_blits_or_cursor_move() {
    // Combined `a=T,U=1` with payload: decodes, stores, registers the
    // prototype with decoded dims — and still places no blit and moves
    // no cursor.
    let mut rt = make_runtime();
    let seq = format!(
        "\x1b_Gf=32,s=2,v=2,a=T,U=1,i=42,c=4,r=2,m=0;{}\x1b\\",
        red_2x2_b64()
    );
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1, "payload stored");
    assert_eq!(rt.kitty_virtual_count(), 1, "prototype registered");
    assert_eq!(rt.kitty_placement_count(), 0, "no blit placed");
    assert_eq!(rt.state().cursor().position.col, 0);
    assert_eq!(rt.state().cursor().position.row, 0);
    print_run_42(&mut rt);
    let rects = rt.kitty_unicode_run_rects_on_row(0, metrics_9x19());
    assert_eq!(rects.len(), 1);
    let _ = rt.tick();
    assert_eq!(rt.kitty_last_frame_images(), 0, "zero blits");
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_scroll_moves_both_layers() {
    // The prototype survives scroll (terminal truth spares anchorless
    // prototypes) and the run rect tracks content through the grid:
    // after 3 scrolled lines the run sits 3 rows higher.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    rt.handle_pty_bytes(b"\x1b[11;1H");
    print_run_42(&mut rt);
    assert_eq!(rt.kitty_unicode_runs_on_row(10).len(), 1);
    rt.handle_pty_bytes(b"\x1b[24;1H");
    rt.handle_pty_bytes(b"\n\n\n");
    assert_eq!(rt.state().scrollback_len(), 3);
    assert!(
        rt.state().kitty_placements().get(42, 0).is_some(),
        "truth prototype survives scroll"
    );
    assert_eq!(rt.kitty_virtual_count(), 1, "registry prototype survives");
    assert_eq!(
        rt.kitty_unicode_runs_on_row(7).len(),
        1,
        "run moved with text"
    );
    let rects = rt.kitty_unicode_run_rects_on_row(7, metrics_9x19());
    assert_eq!(rects.len(), 1);
    assert_eq!((rects[0].grid_row, rects[0].grid_col), (7, 0));
    assert_eq!(rects[0].rect.y, 7 * 19, "no double-counted scroll delta");
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_delete_clears_both_layers() {
    // `a=d,d=i,i=42` clears the blit, the rich prototype, the
    // terminal-truth prototype, and the named grid runs together.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=42,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    // The display above advanced the cursor: home it so the run prints
    // on row 0.
    rt.handle_pty_bytes(b"\x1b[H");
    print_run_42(&mut rt);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(rt.kitty_virtual_count(), 1);
    assert_eq!(rt.kitty_unicode_runs_on_row(0).len(), 1);
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=42\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 0, "blit cleared");
    assert_eq!(rt.kitty_virtual_count(), 0, "prototype cleared");
    assert!(
        rt.state().kitty_placements().get(42, 0).is_none(),
        "truth prototype cleared"
    );
    assert!(
        rt.kitty_unicode_runs_on_row(0).is_empty(),
        "grid runs cleared"
    );
    assert!(
        rt.kitty_unicode_run_rects_on_row(0, metrics_9x19())
            .is_empty(),
        "nothing resolves after delete"
    );
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_over_cap_evicts_oldest_within_origin() {
    // 33 bodiless prototypes on one origin: the shared 32-per-origin
    // budget evicts the oldest prototype first (same-kind FIFO).
    let mut rt = make_runtime();
    for i in 1..=33u32 {
        let seq = format!("\x1b_Ga=p,U=1,i={i},c=4,r=2\x1b\\");
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert_eq!(rt.kitty_virtual_count(), 32);
    // Image 1 lost its prototype: its run still decodes as text but
    // resolves to no rect.
    rt.handle_pty_bytes("\x1b[38;5;1m\u{10EEEE}\u{0305}\u{0305}\x1b[39m".as_bytes());
    assert_eq!(
        rt.kitty_unicode_runs_on_row(0).len(),
        1,
        "run still decodes as text"
    );
    assert!(
        rt.kitty_unicode_run_rects_on_row(0, metrics_9x19())
            .is_empty(),
        "evicted prototype resolves nothing"
    );
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_alt_screen_suppresses_registration() {
    // Alternate screen: bodiless `a=p,U=1` registers nothing rich-side,
    // and combined `a=T,U=1` stores without registering (existing
    // suppression posture). Back on main, registration works.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b[?1049h");
    assert!(rt.state().alt_screen_active());
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    assert_eq!(rt.kitty_virtual_count(), 0, "alt suppresses registration");
    let seq = format!(
        "\x1b_Gf=32,s=2,v=2,a=T,U=1,i=43,c=4,r=2,m=0;{}\x1b\\",
        red_2x2_b64()
    );
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1, "alt stores payload");
    assert_eq!(rt.kitty_virtual_count(), 0, "alt registers nothing");
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b[?1049l");
    assert!(!rt.state().alt_screen_active());
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    assert_eq!(rt.kitty_virtual_count(), 1);
    assert!(rt.state().check_invariants().is_ok());
}

#[test]
fn virtual_ed2_clears_both_layers() {
    // `ED 2` full clear drops blits, prototypes, and grid runs (the
    // erase takes the cells; the origin clears take the registry).
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=42,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    rt.handle_pty_bytes(b"\x1b_Ga=p,U=1,i=42,c=4,r=2\x1b\\");
    // The display above advanced the cursor: home it so the run prints
    // on row 0.
    rt.handle_pty_bytes(b"\x1b[H");
    print_run_42(&mut rt);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(rt.kitty_virtual_count(), 1);
    rt.handle_pty_bytes(b"\x1b[2J");
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(rt.kitty_virtual_count(), 0);
    assert!(rt.kitty_unicode_runs_on_row(0).is_empty());
    assert!(rt.state().check_invariants().is_ok());
}
