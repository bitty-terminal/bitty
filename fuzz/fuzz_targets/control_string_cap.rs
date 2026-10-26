//! `control_string_cap`: control strings at and past the parser's byte cap.
//!
//! The parser-wide cap ([`bitty_vt::CONTROL_STRING_MAX_BYTES`], ~4 MiB) is
//! unreachable from the other targets: they truncate input to
//! [`common::MAX_INPUT_BYTES`] (32 KiB) so a mutation can never grow a string
//! past the cap. Committing over-cap seeds to the retained corpora would hit
//! the same truncation, so the checked-in `generate_cap_seeds` test
//! (`crates/bitty-vt/tests/control_string_cap.rs`, ignored by default) writes
//! them into the gitignored `fuzz/corpus/control_string_cap/` instead. The
//! exact at-cap/just-past-cap boundary is also pinned deterministically by
//! the other tests in that file.
//!
//! Input is fed whole: no truncation and no byte-wise replay (parsing 4 MiB
//! one byte per call is far too slow to fuzz). Fresh-run determinism is still
//! asserted, so a refusal that depends on chunking would surface here.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    common::assert_whole_run_invariants(data);
});
