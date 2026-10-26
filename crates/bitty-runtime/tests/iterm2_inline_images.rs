//! iTerm2 inline images (`OSC 1337 ; File=`) live-path suite (CTX-1590,
//! issue #2217).
//!
//! The parser pre-scans the OSC (which `vte` would otherwise buffer without
//! a cap), bounds and decodes the payload, validates the PNG `IHDR`, and
//! emits `Iterm2Image`; the runtime routes it to the same
//! store/placement/present seam as kitty graphics. These tests prove the
//! anchor cell rect, the cursor-hold rule, the counted fail-closed refusals,
//! chunk-boundary robustness, and that kitty graphics does not regress.

use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 1x1 opaque red PNG (`bitty-rich::kitty_decode` fixture bytes).
const PNG_1X1: [u8; 70] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// Minimal standard-alphabet base64 encoder for fixtures.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(first >> 2) as usize] as char);
        out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// One `OSC 1337 ; File=` request, BEL-terminated.
fn osc1337(header: &str, payload: &[u8]) -> Vec<u8> {
    let mut wire = format!("\x1b]1337;File={header}:{}", base64_encode(payload)).into_bytes();
    wire.push(0x07);
    wire
}

#[test]
fn inline_image_lands_on_the_cursor_cell_rect() {
    let mut rt = make_runtime();
    // Cursor to row 3, column 5 (1-based) => 0-based (col 4, row 2).
    rt.handle_pty_bytes(b"\x1b[3;5H");
    rt.handle_pty_bytes(&osc1337("inline=1;width=2;height=2", &PNG_1X1));
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    assert_eq!(
        rt.kitty_placement_spans(),
        vec![(4, 2, 2, 2)],
        "the image must anchor at the cursor and occupy the requested 2x2 cells"
    );
    assert!(rt.tick().is_some(), "a new placement forces a present");
    assert_eq!(rt.kitty_last_frame_images(), 1);
    // iTerm2 inline images never advance the cursor (`C=1` on the seam).
    let cursor = rt.state().cursor().position;
    assert_eq!((cursor.col, cursor.row), (4, 2));
}

#[test]
fn auto_size_derives_cells_from_pixel_dimensions() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(&osc1337("inline=1;width=auto;height=auto", &PNG_1X1));
    assert_eq!(rt.kitty_image_count(), 1);
    // A 1x1 px image derives one cell on each axis, exactly like kitty's
    // `c=`/`r=` derivation.
    assert_eq!(rt.kitty_placement_spans(), vec![(0, 0, 1, 1)]);
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 1);
}

#[test]
fn osc1337_split_across_reads_is_admitted() {
    let mut rt = make_runtime();
    // Three-byte reads split the introducer, the prefix, the header, and the
    // base64 payload across `advance` calls.
    for chunk in osc1337("inline=1;width=1", &PNG_1X1).chunks(3) {
        rt.handle_pty_bytes(chunk);
    }
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_spans(), vec![(0, 0, 1, 1)]);
}

#[test]
fn oversize_payload_fails_closed_and_is_counted() {
    let mut rt = make_runtime();
    let mut wire = b"\x1b]1337;File=inline=1:".to_vec();
    wire.extend(std::iter::repeat_n(
        b'A',
        bitty_vt::ITERM2_OSC_MAX_BODY_BYTES + 8,
    ));
    wire.push(0x07);
    rt.handle_pty_bytes(&wire);
    assert_eq!(rt.kitty_image_count(), 0, "nothing may be stored");
    assert_eq!(rt.kitty_placement_count(), 0, "nothing may be placed");
    assert_eq!(rt.kitty_last_frame_images(), 0, "nothing may be painted");
    assert_eq!(
        rt.state().telemetry().iterm2_refused,
        1,
        "the refused request must be counted exactly once"
    );
}

#[test]
fn unsupported_format_fails_closed_and_is_counted() {
    let mut rt = make_runtime();
    // JPEG SOI + JFIF marker: a real image format, just not an admitted one.
    rt.handle_pty_bytes(&osc1337("inline=1", b"\xFF\xD8\xFF\xE0not-a-png"));
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
    assert_eq!(rt.state().telemetry().iterm2_refused, 1);
}

#[test]
fn transmit_only_form_fails_closed_and_is_counted() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(&osc1337("inline=0", &PNG_1X1));
    assert_eq!(
        rt.kitty_image_count(),
        0,
        "transmit-only stores no unreachable image"
    );
    assert_eq!(rt.state().telemetry().iterm2_refused, 1);
}

#[test]
fn downstream_decode_failure_is_counted_as_a_refusal() {
    let mut rt = make_runtime();
    // `IHDR` only: the parser admits it (valid PNG signature, `IHDR` chunk,
    // in-cap extents) and the decoder refuses it (no image data), so the
    // refusal happens after parsing and must still be counted.
    rt.handle_pty_bytes(&osc1337("inline=1", &PNG_1X1[..33]));
    assert_eq!(rt.kitty_image_count(), 0, "nothing may be stored");
    assert_eq!(rt.kitty_placement_count(), 0, "nothing may be placed");
    assert_eq!(rt.kitty_last_frame_images(), 0, "nothing may be painted");
    assert_eq!(
        rt.state().telemetry().iterm2_refused,
        1,
        "a downstream decode refusal is still an iTerm2 refusal"
    );
}

#[test]
fn kitty_graphics_still_works_alongside_osc_1337() {
    let mut rt = make_runtime();
    // 2x2 opaque red RGBA (`f=32`).
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=2,v=2,m=0;/wAA//8AAP//AAD//wAA/w==\x1b\\");
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);

    rt.handle_pty_bytes(b"\x1b[5;1H");
    rt.handle_pty_bytes(&osc1337("inline=1;width=1", &PNG_1X1));
    assert_eq!(rt.kitty_image_count(), 2);
    assert_eq!(
        rt.kitty_placement_spans(),
        vec![(0, 0, 1, 1), (0, 4, 1, 1)],
        "kitty keeps its derived cell span; the iTerm2 image anchors at row 5"
    );
    assert!(rt.tick().is_some());
    assert_eq!(rt.kitty_last_frame_images(), 2);
}
