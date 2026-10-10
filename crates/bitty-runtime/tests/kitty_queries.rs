//! Kitty `a=q` query suite (S2 of #1849, Task CTX-1096).
//!
//! The parser reassembles the single-shot query and validates its shape
//! (`f=` mandatory, `q=` 0/1/2); the runtime answers with at most one
//! bounded fixed-format `APC G` reply (`OK`/`ERROR`, always < 1 KiB)
//! queued through the bounded `Reply` queue, honoring wire `q=`
//! suppression. Queries never store or place: support probes test-load
//! through the declared-size pre-check, and image/placement status
//! resolves against the S5 quota-scoped store for the draining origin
//! (mirroring protocol-deletion identity).

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
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=100,OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0, "probes never store");
    assert_eq!(rt.kitty_placement_count(), 0, "probes never place");
}

#[test]
fn query_probe_raw_ok_replies_exact() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,a=q,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=32,OK\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 0);
}

#[test]
fn query_probe_failures_reply_error_exact() {
    // Unknown format.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=7,s=2,v=2,a=q,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=7,ERROR\x1b\\".to_vec()]);
    // Length mismatch (2x2 RGBA needs 16 bytes; 4 given).
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=2,v=2,a=q,m=0;AAAAAA==\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=32,ERROR\x1b\\".to_vec()]);
    // Empty payload carries no image.
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,s=2,v=2,a=q,m=0;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=32,ERROR\x1b\\".to_vec()]);
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
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11,s=2,v=2,OK\x1b\\".to_vec()]
    );
    assert_eq!(rt.kitty_image_count(), 1, "queries store nothing");
    assert_eq!(rt.kitty_placement_count(), 1, "queries place nothing");
}

#[test]
fn query_unknown_image_replies_error_exact() {
    let mut rt = make_runtime();
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=99;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=99,ERROR\x1b\\".to_vec()]);
}

#[test]
fn query_placement_pin_ok_and_unknown_pin_error() {
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=32,s=2,v=2,i=11,p=5,m=0;{}\x1b\\", red_2x2_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(rt.kitty_placement_count(), 1);
    rt.take_replies();
    // Pinned placement held.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11,p=5;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11,p=5,s=2,v=2,OK\x1b\\".to_vec()]
    );
    // Image-level query (no pin) still answers OK.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11,s=2,v=2,OK\x1b\\".to_vec()]
    );
    // Unheld pin answers ERROR.
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=11,p=9;\x1b\\");
    assert_eq!(
        replies(&mut rt),
        vec![b"\x1b_Gi=11,p=9,ERROR\x1b\\".to_vec()]
    );
}

#[test]
fn query_deleted_image_answers_error() {
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
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=11,ERROR\x1b\\".to_vec()]);
}

#[test]
fn query_quiet_suppression_matrix() {
    // q=0 replies to everything.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,q=0,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=100,OK\x1b\\".to_vec()]);
    rt.handle_pty_bytes(b"\x1b_Gf=7,a=q,q=0,m=0;AAAA\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=7,ERROR\x1b\\".to_vec()]);
    // q=1 suppresses OK but not failures.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,q=1,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty(), "q=1 suppresses OK");
    rt.handle_pty_bytes(b"\x1b_Gf=7,a=q,q=1,m=0;AAAA\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=7,ERROR\x1b\\".to_vec()]);
    rt.handle_pty_bytes(b"\x1b_Gf=32,a=q,i=99,q=1;\x1b\\");
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gi=99,ERROR\x1b\\".to_vec()]);
    // q=2 suppresses failures too (silence either way).
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,q=2,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert!(replies(&mut rt).is_empty(), "q=2 suppresses OK");
    rt.handle_pty_bytes(b"\x1b_Gf=7,a=q,q=2,m=0;AAAA\x1b\\");
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
    // Each probe reply is 13 bytes; the 4 KiB reply queue holds 315 whole
    // replies (315 * 13 = 4095). Past the cap the reply drops whole and
    // raises the overflow flag — never a partial answer.
    let mut rt = make_runtime();
    let seq = format!("\x1b_Gf=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    for _ in 0..400 {
        rt.handle_pty_bytes(seq.as_bytes());
    }
    assert!(rt.replies_overflowed(), "over-cap queries must flag");
    let drained = rt.take_replies();
    assert_eq!(drained.len(), 315, "only whole replies are kept");
    for reply in &drained {
        assert_eq!(reply.as_ref(), b"\x1b_Gf=100,OK\x1b\\");
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
    let seq = format!("\x1b_Gf=100,a=q,m=0;{}\x1b\\", red_1x1_png_b64());
    rt.handle_pty_bytes(seq.as_bytes());
    assert_eq!(replies(&mut rt), vec![b"\x1b_Gf=100,ERROR\x1b\\".to_vec()]);
    assert_eq!(rt.kitty_image_count(), 64, "probes store nothing");
}
