//! `osc_string`: OSC payload bytes (`ESC ] ... BEL` / `ESC ] ... ST`).
//!
//! The fuzzer input is used as the OSC body of an otherwise fixed envelope,
//! so mutations target the payload parser (`osc_dispatch`: title, dynamic
//! color, palette, clipboard, cwd, hyperlink, prompt mark, unknown) rather
//! than the introducer. Two envelopes (BEL and `ST` terminators) and two
//! replay invariants are checked per input. Each envelope is replayed both at
//! the production ledger and at a small ledger cap so the raw-`APC`
//! cap-rejection paths are genuinely exercised on framed bytes.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for frame in common::osc_frames(data) {
        common::assert_parser_invariants(&frame);
        common::assert_small_ledger_invariants(&frame);
    }
});
