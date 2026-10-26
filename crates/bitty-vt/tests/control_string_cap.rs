//! CTX-1592 (bitty#2704): parser-wide control-string growth bound.
//!
//! `vte`'s `std` OSC collector is a growable `Vec` with no cap
//! (`MAX_OSC_RAW` exists only in its `no_std` build), so an unterminated
//! `OSC` — or a string interleaved with an `APC` — grew memory 1:1 with
//! input. Before this fix, feeding 256 MiB of any of these shapes peaked at
//! 512 MiB–1 GiB of live heap.
//!
//! These tests feed 256 MiB of each adversarial shape through the public
//! parser API and measure the real heap peak with a per-thread tracking
//! allocator (the pattern established by
//! `crates/bitty-rich/tests/background_peak_memory.rs`), so the assertion is
//! a byte bound, not a duration. They also assert the counted refusal and
//! that output after the refused string still arrives.
//!
//! Each shape is expected to produce exactly one
//! [`TerminalAction::ControlStringRejected`]. That action does not exist on
//! the pre-fix parser, and the peak bound is far below the pre-fix growth, so
//! these tests fail against `main`'s pre-fix `parser.rs`.

// The workspace lint level is `deny`; this test needs `unsafe` only for the
// tracking `GlobalAlloc` that measures the parse peak.
#![allow(unsafe_code)]

use bitty_vt::{ControlStringKind, ControlStringReject, Parser, TerminalAction};

/// Per-thread heap accounting, copied from
/// `crates/bitty-rich/tests/background_peak_memory.rs`: only allocations on
/// the measuring thread are counted, so the parallel test harness cannot
/// contaminate the peak.
mod peak_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static LIVE: Cell<usize> = const { Cell::new(0) };
        static PEAK: Cell<usize> = const { Cell::new(0) };
    }

    struct Scope;

    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.with(|active| active.set(false));
        }
    }

    /// Global allocator that records this thread's live and peak heap bytes
    /// while a measurement scope is active.
    pub struct PeakAllocator;

    unsafe impl GlobalAlloc for PeakAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                record_alloc(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            record_dealloc(layout.size());
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                record_alloc(layout.size());
            }
            ptr
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                record_dealloc(layout.size());
                record_alloc(new_size);
            }
            new_ptr
        }
    }

    fn record_alloc(size: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                LIVE.with(|live| {
                    let next = live.get().saturating_add(size);
                    live.set(next);
                    PEAK.with(|peak| {
                        if next > peak.get() {
                            peak.set(next);
                        }
                    });
                });
            }
        });
    }

    fn record_dealloc(size: usize) {
        ACTIVE.with(|active| {
            if active.get() {
                LIVE.with(|live| live.set(live.get().saturating_sub(size)));
            }
        });
    }

    /// Runs `f` while measuring this thread's heap, returning its result and
    /// the peak live-bytes observed.
    pub fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        ACTIVE.with(|active| active.set(false));
        LIVE.with(|live| live.set(0));
        PEAK.with(|peak| peak.set(0));
        ACTIVE.with(|active| active.set(true));
        let scope = Scope;
        let out = f();
        drop(scope);
        (out, PEAK.with(|peak| peak.get()))
    }
}

#[global_allocator]
static PEAK_ALLOCATOR: peak_alloc::PeakAllocator = peak_alloc::PeakAllocator;

const MIB: usize = 1024 * 1024;

/// Adversarial length fed to each shape (the issue's measured input).
const TOTAL: usize = 256 * MIB;

/// Feed granularity; the cap is a byte count, so the chunk size is
/// irrelevant to where the refusal lands.
const CHUNK: usize = 64 * 1024;

/// Parser cap, spelled from the public iTerm2 constants so this test also
/// compiles against the pre-fix parser (where
/// `bitty_vt::CONTROL_STRING_MAX_BYTES` does not exist). A unit test in
/// `parser::tests` pins the two values equal.
const CAP: usize = bitty_vt::ITERM2_OSC_MAX_BODY_BYTES + bitty_vt::ITERM2_OSC_PREFIX.len() + 2;

/// Hard heap bound for one 256 MiB shape. Post-fix the peak is the cap plus
/// `vte`'s one capacity doubling and one bounded dispatch copy (~16 MiB for
/// an OSC) — or the existing kitty/iTerm2 budgets, whichever applies. The
/// pre-fix parser reached 512 MiB–1 GiB for the same input.
const PEAK_BOUND: usize = 32 * MIB;

struct Outcome {
    peak: usize,
    actions: Vec<TerminalAction>,
}

/// Feeds `prefix`, then `total` bytes in `CHUNK` slices of `chunk`, then
/// `suffix`, measuring the thread's heap peak.
fn feed(prefix: &[u8], chunk: &[u8], total: usize, suffix: &[u8]) -> Outcome {
    let (actions, peak) = peak_alloc::measure(|| {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        parser.advance(prefix, |action| actions.push(action));
        let mut sent = 0;
        while sent < total {
            let n = chunk.len().min(total - sent);
            parser.advance(&chunk[..n], |action| actions.push(action));
            sent += n;
        }
        parser.advance(suffix, |action| actions.push(action));
        actions
    });
    Outcome { peak, actions }
}

/// Counted refusal kinds in action order; every one carries the stable
/// overflow reason.
fn refusal_kinds(actions: &[TerminalAction]) -> Vec<ControlStringKind> {
    actions
        .iter()
        .filter_map(|action| match action {
            TerminalAction::ControlStringRejected { kind, reason } => {
                assert_eq!(*reason, ControlStringReject::Overflow);
                Some(*kind)
            }
            _ => None,
        })
        .collect()
}

/// Asserts the memory bound for one 256 MiB shape.
fn assert_bounded(outcome: &Outcome, shape: &str) {
    eprintln!("{shape}: peak {} bytes", outcome.peak);
    assert!(
        outcome.peak <= PEAK_BOUND,
        "{shape}: peak heap {} bytes exceeds the {PEAK_BOUND}-byte bound",
        outcome.peak
    );
}

/// Asserts the stream ends with the `ok` print pair (recovery after the cap).
fn assert_recovers(actions: &[TerminalAction], shape: &str) {
    assert_eq!(
        &actions[actions.len().saturating_sub(2)..],
        &[
            TerminalAction::Print(bitty_vt::GraphemeCell::from('o')),
            TerminalAction::Print(bitty_vt::GraphemeCell::from('k')),
        ],
        "{shape}: output after the refused string must still arrive"
    );
}

#[test]
fn bare_unterminated_osc_is_bounded_and_reported() {
    let chunk = vec![b'A'; CHUNK];
    let out = feed(b"\x1b]0;", &chunk, TOTAL, b"\x07ok");
    assert_bounded(&out, "bare OSC");
    assert_eq!(refusal_kinds(&out.actions), vec![ControlStringKind::Osc]);
    assert_recovers(&out.actions, "bare OSC");
}

#[test]
fn apc_inside_unterminated_osc_is_bounded_and_reported() {
    // Shape 2: the parser consumes `ESC _`, so `vte`'s OSC stays open; the
    // text after the APC is what grows the collector before this fix.
    let chunk = vec![b'A'; CHUNK];
    let out = feed(b"\x1b]0;t\x1b_Gm=0;\x1b\\", &chunk, TOTAL, b"\x07ok");
    assert_bounded(&out, "APC inside OSC");
    assert_eq!(refusal_kinds(&out.actions), vec![ControlStringKind::Osc]);
    assert_recovers(&out.actions, "APC inside OSC");
}

#[test]
fn kitty_apc_interleaved_with_text_is_bounded_and_reported() {
    // Shape 3: a chunked kitty stream whose payload is interleaved with
    // non-base64 text. The text aborts the pending transmission and the
    // remaining APC bytes are charged to the shared control-string budget.
    let chunk = b"!!!! text !!!! ".repeat(CHUNK / 16 + 1);
    let out = feed(b"\x1b_Ga=T,f=32,s=1,v=1,m=1;AAAA", &chunk, TOTAL, b"\x07ok");
    assert_bounded(&out, "kitty APC interleaved");
    assert_eq!(refusal_kinds(&out.actions), vec![ControlStringKind::Apc]);
    assert_recovers(&out.actions, "kitty APC interleaved");
}

#[test]
fn image_diversion_after_a_capped_string_is_admitted() {
    // A valid iTerm2 image after an over-cap unterminated OSC must still be
    // diverted and admitted; the refusal does not poison the next sequence.
    let chunk = vec![b'A'; CHUNK];
    let suffix = format!("\x1b]1337;File=inline=1:{}\x07ok", base64(&PNG_1X1));
    let out = feed(b"\x1b]0;", &chunk, TOTAL, suffix.as_bytes());
    assert_bounded(&out, "image after capped OSC");
    assert_eq!(refusal_kinds(&out.actions), vec![ControlStringKind::Osc]);
    assert!(
        out.actions
            .iter()
            .any(|action| matches!(action, TerminalAction::Iterm2Image { .. })),
        "the image after the capped string must be admitted"
    );
    assert_recovers(&out.actions, "image after capped OSC");
}

#[test]
fn the_iterm2_diversion_shares_the_cap_without_being_refused() {
    // The shared cap is sized by the iTerm2 maximum: a candidate body at the
    // iTerm2 cap consumes the whole parser budget exactly and is still
    // refused by the image scanner (OversizePayload), never by the
    // control-string cap.
    let mut wire = b"\x1b]1337;File=inline=1:".to_vec();
    wire.extend(std::iter::repeat_n(
        b'A',
        bitty_vt::ITERM2_OSC_MAX_BODY_BYTES,
    ));
    wire.push(0x07);
    let actions = {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        for chunk in wire.chunks(CHUNK) {
            parser.advance(chunk, |action| actions.push(action));
        }
        actions
    };
    assert!(
        actions
            .iter()
            .all(|action| !matches!(action, TerminalAction::ControlStringRejected { .. })),
        "the shared cap must admit the iTerm2 maximum: {actions:?}"
    );
    assert!(matches!(
        actions.as_slice(),
        [TerminalAction::Iterm2ImageRejected {
            reason: bitty_vt::Iterm2Reject::OversizePayload
        }]
    ));
}

#[test]
fn the_cap_lands_at_the_same_offset_however_the_stream_is_split() {
    let prefix = b"\x1b]0;";
    let mut stream = prefix.to_vec();
    stream.extend(std::iter::repeat_n(b'A', CAP + 8 * 1024));
    stream.extend_from_slice(b"\x07ok");

    let whole = {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        parser.advance(&stream, |action| actions.push(action));
        actions
    };
    assert_eq!(refusal_kinds(&whole), vec![ControlStringKind::Osc]);
    assert_recovers(&whole, "cap split invariance");

    let mut cuts = vec![
        0,
        1,
        prefix.len(),
        prefix.len() + CAP - 1,
        prefix.len() + CAP,
        prefix.len() + CAP + 1,
        prefix.len() + CAP + 2,
        stream.len() / 2,
        stream.len() - 4,
        stream.len() - 1,
    ];
    cuts.sort_unstable();
    cuts.dedup();
    for cut in cuts {
        let mut parser = Parser::new();
        let mut actions = Vec::new();
        parser.advance(&stream[..cut], |action| actions.push(action));
        parser.advance(&stream[cut..], |action| actions.push(action));
        assert_eq!(actions, whole, "split at offset {cut}");
    }
}

/// 1x1 RGBA PNG used only to prove the image diversion still works.
const PNG_1X1: [u8; 70] = [
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 240, 31, 0,
    5, 0, 1, 255, 86, 199, 47, 13, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(first >> 2) as usize] as char);
        out.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
