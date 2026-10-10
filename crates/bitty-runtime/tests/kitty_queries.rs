//! Kitty `a=q` query suite (S2 of #1849, Task CTX-1096).
//!
//! Spec-exact wire shape (`recording/references/kitty@0b12ed8`,
//! GPL-3.0-only): replies use the `;` separator with `OK` or `CODE:msg`
//! verdicts (`docs/graphics-protocol.rst:430,476,480,500`,
//! `kitty/graphics.c:927-951`, `kitty_tests/graphics.py:40-52`), echo
//! identity keys (`i=`/`I=`/`p=`) and never `s=`/`v=`/`f=`.
//!
//! The parser reassembles the single-shot query and validates its shape
//! (`f=` mandatory, `q=` 0/1/2); the runtime answers support probes
//! (`i=` plus payload bytes, test-loaded without storing) and
//! payload-less status lookups (`i=`/`I=`/`p=` with empty payload, bitty
//! extension in spec shape — kitty-native clients always send payload
//! plus `i=`, so they never hit the extension path) with at most one
//! bounded reply queued through the bounded `Reply` queue, honoring wire
//! `q=` suppression. Queries without `i=`/`I=` stay silent like kitty's
//! `REPORT_ERROR`-with-no-reply. Queries never store or place.

use bitty_runtime::Runtime;

fn make_runtime() -> Runtime {
    Runtime::with_defaults().expect("defaults must build")
}

/// 2x2 opaque red RGBA payload (`f=32`), base64 `/wAA//8AAP//AAD//wAA/w==`.
fn red_2x2_b64() -> &'static str {
    "/wAA//8AAP//AAD//wAA/w=="
}

/// 1x1 opaque red RGBA PNG (`f=100`), base64 of the `bitty-rich` fixture.
fn red_1x1_png_b64() -> &'static str {
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFAAH/VscvDQAAAABJRU5ErkJggg=="
}

fn replies(rt: &mut Runtime) -> Vec<Vec<u8>> {
    rt.take_replies()
        .iter()
        .map(|reply| reply.to_vec())
        .collect()
}

#[test]
fn query_probe_png_ok_replies_exact_and_stores_nothing() {
    // Spec support probe (`docs/graphics-protocol.rst:457` shape with an
    // id): the reply echoes the queried `i=` with `;OK`.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0, "probes never store");
    assert_eq!(rt.kitty_placement_count(), 0, "probes never place");
}

#[test]
fn query_probe_raw_ok_replies_exact() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=32,s=2,v=2,a=q,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0);
}

#[test]
fn query_probe_without_id_is_silent() {
    // Spec-exact: kitty mandates the id on queries (`kitty/graphics.c`
    // `REPORT_ERROR` when `!q_iid`, no reply). No `f=`-echo extension is
    // kept: an `f=`-echo shape would break the client's `partition(';')`
    // parser with zero interop value.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty());
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,a=q,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn query_probe_failures_reply_code_colon_msg_exact() {
    // Unknown format answers `EINVAL` with the echoed `i=`.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=7,s=2,v=2,a=q,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=31;EINVAL:unknown format\x1b\\".to_vec()]
    );
    // Length mismatch (2x2 RGBA needs 16 bytes; 4 given).
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gi=31,f=32,s=2,v=2,a=q,m=0;AAAAAA==\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=31;EINVAL:bad data\x1b\\".to_vec()]
    );
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn query_image_status_ok_replies_exact() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=11,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_image_count(), 1);
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.take_replies();
    // Payload-less status lookup (bitty extension in spec shape): the
    // reply echoes `i=` with `;OK`, never `s=`/`v=`/`f=`.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=11;OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 1, "queries store nothing");
    assert_eq!(rt.kitty_placement_count(), 1, "queries place nothing");
}

#[test]
fn query_unknown_image_replies_enoent_exact() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=99;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=99;ENOENT:not held\x1b\\".to_vec()]
    );
}

#[test]
fn query_unknown_number_replies_enoent_with_number_echo() {
    // `I=` numbers resolve through terminal truth; an unresolvable
    // number echoes the queried `I=` with `ENOENT`.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,I=77;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_GI=77;ENOENT:not held\x1b\\".to_vec()]
    );
}

#[test]
fn query_placement_pin_ok_and_unknown_pin_error() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=11,p=5,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.take_replies();
    // Pinned placement held: `Gi=<id>,p=<pin>;OK` (spec `:500` shape).
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11,p=5;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=11,p=5;OK\x1b\\".to_vec()]);
    // Image-level query (no pin) still answers OK.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=11;OK\x1b\\".to_vec()]);
    // Unheld pin answers `ENOENT`.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11,p=9;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11,p=9;ENOENT:not held\x1b\\".to_vec()]
    );
}

#[test]
fn query_deleted_image_answers_enoent() {
    // Status resolves rendered placements (the same identity protocol
    // deletion clears): deleting the placement makes the wire id
    // unaddressable even though the decoded bytes stay inertly stored.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=11,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    rt.take_replies();
    rt.handle_pty_bytes(b"\x1b_Ga=d,d=i,i=11\x1b\\");
    assert_eq!(rt.kitty_placement_count(), 0);
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11;ENOENT:not held\x1b\\".to_vec()]
    );
}

#[test]
fn query_quiet_suppression_matrix() {
    // q=0 replies to everything (quiet logic unchanged: 1 suppresses OK
    // only, >1 suppresses all — kitty `finish_command_response` parity).
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=100,a=q,q=0,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
    rt.handle_pty_bytes(b"\x1b_Gi=31,f=7,a=q,q=0,m=0;AAAA\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=31;EINVAL:unknown format\x1b\\".to_vec()]
    );
    // q=1 suppresses OK but not failures.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=100,a=q,q=1,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty(), "q=1 suppresses OK");
    rt.handle_pty_bytes(b"\x1b_Gi=31,f=7,a=q,q=1,m=0;AAAA\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=31;EINVAL:unknown format\x1b\\".to_vec()]
    );
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=99,q=1;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=99;ENOENT:not held\x1b\\".to_vec()]
    );
    // q=2 suppresses failures too (silence either way).
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=100,a=q,q=2,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty(), "q=2 suppresses OK");
    rt.handle_pty_bytes(b"\x1b_Gi=31,f=7,a=q,q=2,m=0;AAAA\x1b\\");
    assert!(replies(&mut rt).is_empty(), "q=2 suppresses failures");
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=99,q=2;\x1b\\");
    assert!(replies(&mut rt).is_empty());
}

#[test]
fn query_missing_format_is_silent() {
    // A bodiless `a=q` without `f=` stays `MissingFormat` at the parser:
    // no action, no reply, no storage.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Ga=q,i=5\x1b\\");
    assert!(replies(&mut rt).is_empty());
    assert_eq!(rt.kitty_image_count(), 0);
    assert_eq!(rt.kitty_placement_count(), 0);
}

#[test]
fn query_over_cap_drops_whole_reply() {
    // Each probe reply is 12 bytes (`ESC _ G i=31 ; O K ESC \`); the 4
    // KiB reply queue holds 341 whole replies (341 * 12 = 4092). Past the
    // cap the reply drops whole and raises the overflow flag — never a
    // partial answer.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gi=31,f=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    for _ in 0..400 {
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert!(rt.replies_overflowed(), "over-cap queries must flag");
    let drained = rt.take_replies();
    assert_eq!(drained.len(), 341, "only whole replies are kept");
    for reply in &drained {
        assert_eq!(reply.as_ref(), b"\x1b_Gi=31;OK\x1b\\");
    }
    assert!(!rt.replies_overflowed(), "flag clears with its drain");
}

#[test]
fn query_probe_refuses_when_store_full() {
    // Fill the 64-image global cap with transmit-only 1x1 images (S5
    // per-origin quotas: own-origin FIFO keeps admitting to 64, then the
    // global bound held by this origin refuses). A further probe must
    // report the refused-when-full state instead of OK.
    let mut rt = make_runtime();
    for _ in 0..64 {
        rt.handle_pty_bytes(b"\x1b_Gf=32,s=1,v=1,a=t,m=0;AAAAAA==\x1b\\");
    }
    assert_eq!(rt.kitty_image_count(), 64);
    rt.take_replies();
    let seq = format!("\x1b_Gi=31,f=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=31;ENOSPC:store full\x1b\\".to_vec()]
    );
    assert_eq!(rt.kitty_image_count(), 64, "probes store nothing");
}
