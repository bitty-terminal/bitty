//! `vt_parser`: arbitrary bytes through the public VT parser API.
//!
//! This is the broadest target: any byte stream a PTY could deliver goes
//! straight into [`bitty_vt::Parser::advance`]. It asserts the two replay
//! invariants the parser contract promises (identical fresh-run determinism
//! and chunking identity) and additionally replays under a small kitty ledger
//! cap so the `APC` cap-rejection branches are exercised. A panic or an
//! infinite loop (detected by libFuzzer's default timeout) is a finding.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    common::assert_parser_invariants(data);
    common::assert_small_ledger_invariants(data);
});
