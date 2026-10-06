//! Runtime idle handoff for the shaped stack (issue #1693).
//!
//! Follow-up of #1666 (CTX-0961 F2, PX-4637): hand runtime idle-test
//! coverage to the shaped stack per PX-4614. Test-coverage handoff only —
//! no product-code design changes.
//!
//! What this proves: shaping-trigger content on the grid (programming
//! ligature sequences, the CTX-0961 CJK parity pair, combining-mark and
//! ZWJ clusters) presents exactly the damage it causes and then returns
//! the runtime to frame-on-demand idle (`tick() == None`). Shaping must
//! never arm animations, wakeups, or perpetual damage: the idle resource
//! budget (PB-7, ≤ 1% CPU) depends on the present-then-idle contract, and
//! the render-side shaped suites (`shaped_parity`, `shaped_phase_b`,
//! `shaped_phase_c`) assert pixels while this file asserts the runtime
//! settles.
//!
//! Determinism notes (all tests headless, CI-safe, no sleeps):
//!
//! - The corpus carries no BEL and no OSC sequences, so no time gate arms
//!   (no bell flash, no notification banner, no paste banner, no
//!   synchronized-update defer): the idle assert immediately after the
//!   presenting tick is wall-clock independent. See issue #1711 for why
//!   BEL-corpus tests must instead drain through the virtual-clock seam
//!   (`bell_notification_deadline` + `tick_at`).
//! - The shaped stack itself (`SwashSingle`) stays an explicit opt-in the
//!   runtime does not wire yet (see `Backend selection` in
//!   `bitty-render`'s crate docs); grid truth is shared (`Snapshot` by
//!   shared ref), so the idle contract asserted here is
//!   rasterizer-independent and becomes the landing pad for shaped-wiring
//!   idle assertions when the runtime adopts the shaped stack.
//!
//! ```text
//! cargo test -p bitty-runtime --test shaped_idle
//! ```

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

/// Programming-ligature trigger sequences from the #1666 acceptance
/// corpus (`->`, `!=`, `===` plus the neighboring `PROGRAMMING_LIGATURE_TAGS`
/// shapes). Plain VT-printable bytes: no BEL/OSC, so no time gate arms.
const LIGATURE_ROWS: &[&str] = &[
    "if a != b && c === d {\r\n",
    "let x = a -> b => c;\r\n",
    "a :: b ..= c ||= d;\r\n",
];

/// CTX-0961 CJK parity pair (`U+6F22`/`U+5B57`): double-cell wide spans,
/// the shaped stack's wide-span repaint concern, through the runtime seam.
const CJK_ROW: &str = "\u{6F22}\u{5B57}\r\n";

/// Combining-mark cluster plus a ZWJ sequence plus an emoji-presentation
/// scalar: multi-scalar clusters the shaper groups by byte offset.
const CLUSTER_ROWS: &[&str] =
    &["e\u{301} + \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \u{2764}\u{FE0F}\r\n"];

fn idle_runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("headless runtime must build")
}

/// Feeds `bytes`, asserts the damage presents, then asserts idle.
/// Returns the presenting frame stats for content assertions.
fn feed_present_then_idle(rt: &mut Runtime, bytes: &[u8]) -> bitty_runtime::PresentStats {
    rt.handle_pty_bytes(bytes);
    let presented = rt
        .tick()
        .expect("shaped content damage must present exactly one frame");
    assert!(
        presented.fills > 0 || presented.glyphs > 0,
        "presenting frame must carry fills or glyphs"
    );
    assert_eq!(rt.tick(), None, "runtime must idle after shaped present");
    presented
}

#[test]
fn ligature_trigger_rows_present_then_idle() {
    let mut rt = idle_runtime();
    assert!(rt.tick().is_some(), "initial full redraw must present");
    assert_eq!(rt.tick(), None, "clean grid must idle after present");

    for row in LIGATURE_ROWS {
        feed_present_then_idle(&mut rt, row.as_bytes());
    }
    // Grid truth kept every row: replay the corpus bytes into the snapshot
    // text and confirm all three rows landed (no truncation/panic).
    let text = snapshot_text(&rt);
    for needle in ["!=", "===", "->", "=>", "::", "..="] {
        assert!(
            text.contains(needle),
            "grid truth must keep ligature bytes {needle:?}"
        );
    }
}

#[test]
fn cjk_parity_pair_presents_then_idles() {
    let mut rt = idle_runtime();
    assert!(rt.tick().is_some(), "initial full redraw must present");

    // No trailing newline: the cursor stays on the CJK row so the
    // erase-entire-line below targets the span's own row.
    feed_present_then_idle(&mut rt, "\u{6F22}\u{5B57}".as_bytes());
    // Erase-entire-line over the wide span must also settle: one repaint,
    // then idle. (`CSI 2 K`: the default `CSI K` only clears cursor-to-end
    // and would leave the span's leading cells behind.)
    feed_present_then_idle(&mut rt, b"\x1b[2K");
    let text = snapshot_text(&rt);
    assert!(
        !text.contains('\u{6F22}') && !text.contains('\u{5B57}'),
        "erase-line must clear the CJK span from grid truth"
    );
}

#[test]
fn combining_and_zwj_clusters_present_then_idle() {
    let mut rt = idle_runtime();
    assert!(rt.tick().is_some(), "initial full redraw must present");

    for row in CLUSTER_ROWS {
        feed_present_then_idle(&mut rt, row.as_bytes());
    }
}

#[test]
fn cursor_parked_on_ligature_row_still_idles() {
    // Cursor-policy path (`Cursor` re-shapes the cursor row with ligatures
    // zeroed when the cursor intersects a span): parking the cursor on a
    // ligature row must not produce perpetual damage.
    let mut rt = idle_runtime();
    assert!(rt.tick().is_some(), "initial full redraw must present");

    feed_present_then_idle(&mut rt, LIGATURE_ROWS[0].as_bytes());
    // CUP is 1-based: park on row 1 over the `!=` run.
    rt.handle_pty_bytes(b"\x1b[1;6H");
    // A cursor-only batch may or may not force one repaint; either way the
    // runtime must be idle after at most one more frame.
    let _ = rt.tick();
    assert_eq!(rt.tick(), None, "cursor parked on a ligature row must idle");
    // Re-parking on the same cell must not wake the runtime either.
    rt.handle_pty_bytes(b"\x1b[1;6H");
    let _ = rt.tick();
    assert_eq!(rt.tick(), None, "repeated park must stay idle");
}

#[test]
fn shaped_corpus_replay_is_deterministic_and_idles() {
    // Mirrors the final_integration determinism proof for the shaped
    // corpus: two fresh runtimes fed identical bytes must agree on
    // generation/fills/glyphs plus bit-identical RGBA, and both idle.
    let mut corpus = Vec::new();
    for row in LIGATURE_ROWS {
        corpus.extend_from_slice(row.as_bytes());
    }
    corpus.extend_from_slice(CJK_ROW.as_bytes());
    for row in CLUSTER_ROWS {
        corpus.extend_from_slice(row.as_bytes());
    }

    let mut rt = idle_runtime();
    let mut rt2 = idle_runtime();
    assert!(rt.tick().is_some());
    assert!(rt2.tick().is_some());

    rt.handle_pty_bytes(&corpus);
    rt2.handle_pty_bytes(&corpus);
    let first = rt.tick().expect("corpus damage must present");
    let replay = rt2.tick().expect("replay damage must present");
    assert_eq!(first.generation, replay.generation);
    assert_eq!(first.fills, replay.fills);
    assert_eq!(first.glyphs, replay.glyphs);
    assert_eq!(
        rt.headless_rgba().expect("rgba after corpus"),
        rt2.headless_rgba().expect("rgba after replay"),
        "replay RGBA must be bit-identical"
    );
    assert_eq!(rt.tick(), None, "corpus runtime must idle");
    assert_eq!(rt2.tick(), None, "replay runtime must idle");
}

fn snapshot_text(rt: &Runtime) -> String {
    let snap = rt.snapshot();
    let mut out = String::new();
    for cell in snap.cells.iter() {
        out.push(cell.glyph);
    }
    out
}
