//! `dcs_apc_string`: DCS/APC/SOS/PM string payloads plus kitty `APC G`.
//!
//! The fuzzer input is embedded in fixed string-sequence envelopes: DCS
//! (`ESC P`), APC (`ESC _`), SOS (`ESC X`), and PM (`ESC ^`) with both `ST`
//! and `BEL` terminators, plus kitty graphics shapes (lone single-shot,
//! `m=1`/`m=1`/`m=0` chunked reassembly, and raw-format claims that reach
//! `validate_raw_claim`). Each envelope is replayed for determinism and
//! additionally at a small ledger cap so the `KittyApcAssembler`
//! oversize/chunk-growth rejection paths are reachable from a short input
//! without allocating the production 320 MB ledger.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for frame in common::string_frames(data) {
        common::assert_parser_invariants(&frame);
        common::assert_small_ledger_invariants(&frame);
    }
    for frame in common::kitty_frames(data) {
        common::assert_parser_invariants(&frame);
        common::assert_small_ledger_invariants(&frame);
    }
});
