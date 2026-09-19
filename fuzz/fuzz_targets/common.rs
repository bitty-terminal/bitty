//! Shared helpers for the `bitty-fuzz` targets.
//!
//! Every target feeds adversarial bytes through the real public
//! [`bitty_vt::Parser`] API and drives it to completion. The helpers keep the
//! target-side footprint bounded: input is truncated to [`MAX_INPUT_BYTES`]
//! and collected actions are capped at [`MAX_ACTIONS`], so one fuzz call can
//! never grow without bound relative to its input. The parser holds no
//! terminal state and its own payload bounds (`vte` parameter/OSC limits,
//! `BoundedString`/`BoundedBytes`, the kitty ledger) stay in force.
//!
//! Each target compiles this module independently, so helpers a given target
//! does not call are dead code for that binary only.

#![allow(dead_code)]

use bitty_vt::{Parser, TerminalAction};

/// Hard cap on the bytes any target feeds to a parser per call. All retained
/// seeds are at most 8 KiB, so this never truncates a corpus entry while it
/// still bounds a mutated input.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

/// Hard cap on actions retained per run. Every byte is still parsed; only
/// the retained fingerprint stops growing, keeping the comparison vectors
/// bounded for pathological print-per-byte inputs.
pub const MAX_ACTIONS: usize = 64 * 1024;

/// Small ledger cap used by the string targets so the kitty `m=` reassembly
/// and raw-APC overflow paths are reachable from a short input without
/// allocating the production 320 MB ceiling.
pub const TEST_LEDGER_CAP: usize = 4096;

const ESC: u8 = 0x1B;
const BEL: u8 = 0x07;

fn push_capped(actions: &mut Vec<TerminalAction>, action: TerminalAction) {
    if actions.len() < MAX_ACTIONS {
        actions.push(action);
    }
}

fn collect_whole(bytes: &[u8]) -> Vec<TerminalAction> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    parser.advance(bytes, |action| push_capped(&mut actions, action));
    actions
}

fn collect_bytewise(bytes: &[u8]) -> Vec<TerminalAction> {
    let mut parser = Parser::new();
    let mut actions = Vec::new();
    for byte in bytes {
        parser.advance(std::slice::from_ref(byte), |action| {
            push_capped(&mut actions, action);
        });
    }
    actions
}

fn collect_with_ledger(bytes: &[u8]) -> Vec<TerminalAction> {
    let mut parser = Parser::with_ledger_cap(TEST_LEDGER_CAP);
    let mut actions = Vec::new();
    parser.advance(bytes, |action| push_capped(&mut actions, action));
    actions
}

fn truncate(data: &[u8]) -> &[u8] {
    &data[..data.len().min(MAX_INPUT_BYTES)]
}

/// Asserts the core replay invariants on one byte stream: a fresh parser is
/// deterministic across identical runs, and feeding the same stream byte by
/// byte yields the identical action sequence as one bulk feed.
pub fn assert_parser_invariants(data: &[u8]) {
    let bytes = truncate(data);
    let first = collect_whole(bytes);
    let second = collect_whole(bytes);
    assert_eq!(
        first, second,
        "parser replay diverged between identical runs"
    );
    let chunked = collect_bytewise(bytes);
    assert_eq!(first, chunked, "chunking identity diverged");
}

/// Adds a second, independent invariant for the kitty `APC` paths: replay at
/// a small ledger cap (the production ledger is 320 MB and unreachable from
/// a fuzzer input, so the cap-rejection paths would otherwise stay cold).
pub fn assert_small_ledger_invariants(data: &[u8]) {
    let bytes = truncate(data);
    let first = collect_with_ledger(bytes);
    let second = collect_with_ledger(bytes);
    assert_eq!(
        first, second,
        "ledger-capped parser replay diverged between identical runs"
    );
}

/// Wraps a raw payload as terminal string sequences for every introducer the
/// parser distinguishes: DCS (`ESC P`), APC (`ESC _`), SOS (`ESC X`), and PM
/// (`ESC ^`), each with both `ST` and `BEL` terminators.
pub fn string_frames(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    for introducer in [b'P', b'_', b'X', b'^'] {
        let mut with_st = Vec::with_capacity(payload.len() + 4);
        with_st.push(ESC);
        with_st.push(introducer);
        with_st.extend_from_slice(payload);
        with_st.extend_from_slice(b"\x1b\\");
        frames.push(with_st);

        let mut with_bel = Vec::with_capacity(payload.len() + 3);
        with_bel.push(ESC);
        with_bel.push(introducer);
        with_bel.extend_from_slice(payload);
        with_bel.push(BEL);
        frames.push(with_bel);
    }
    frames
}

/// Wraps a raw payload as kitty graphics `APC` shapes: a lone single-shot, a
/// three-chunk `m=1`/`m=1`/`m=0` reassembly (with a mandatory `f=` on the
/// opening chunk, and a mid-stream chunk that exercises encoded growth), and
/// single-shot packets carrying raw format claims that reach
/// `validate_raw_claim`.
pub fn kitty_frames(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();

    let mut single = Vec::with_capacity(payload.len() + 6);
    single.extend_from_slice(b"\x1b_G");
    single.extend_from_slice(payload);
    single.extend_from_slice(b"\x1b\\");
    frames.push(single);

    // Opening chunk must carry `f=` (`parse_control` rejects a control with
    // no format), so the stream actually opens; the mid and final chunks are
    // continuations where only `m=` is read (`more_flag`).
    let mut first_chunk = Vec::with_capacity(payload.len() + 24);
    first_chunk.extend_from_slice(b"\x1b_Gf=32,s=1,v=1,m=1;");
    first_chunk.extend_from_slice(payload);
    first_chunk.extend_from_slice(b"\x1b\\");
    let mut mid_chunk = Vec::with_capacity(payload.len() + 8);
    mid_chunk.extend_from_slice(b"\x1b_Gm=1;");
    mid_chunk.extend_from_slice(payload);
    mid_chunk.extend_from_slice(b"\x1b\\");
    let mut final_chunk = Vec::with_capacity(payload.len() + 7);
    final_chunk.extend_from_slice(b"\x1b_Gm=0;");
    final_chunk.extend_from_slice(payload);
    final_chunk.extend_from_slice(b"\x1b\\");
    frames.push([first_chunk, mid_chunk, final_chunk].concat());

    let mut raw_claim = Vec::with_capacity(payload.len() + 16);
    raw_claim.extend_from_slice(b"\x1b_Gf=32,s=4096,v=4096;");
    raw_claim.extend_from_slice(payload);
    raw_claim.extend_from_slice(b"\x1b\\");
    frames.push(raw_claim);

    frames
}

/// Wraps a raw payload as OSC sequences (`ESC ] <payload>`), terminated by
/// both BEL and `ST`, the two terminations the parser accepts for OSC.
pub fn osc_frames(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();

    let mut with_bel = Vec::with_capacity(payload.len() + 3);
    with_bel.extend_from_slice(b"\x1b]");
    with_bel.extend_from_slice(payload);
    with_bel.push(BEL);
    frames.push(with_bel);

    let mut with_st = Vec::with_capacity(payload.len() + 4);
    with_st.extend_from_slice(b"\x1b]");
    with_st.extend_from_slice(payload);
    with_st.extend_from_slice(b"\x1b\\");
    frames.push(with_st);

    frames
}
