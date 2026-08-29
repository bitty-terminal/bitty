//! VT parser throughput baseline — PB-6 throughput floor.
//!
//! Headless, bounded, `#![forbid(unsafe_code)]` harness for
//! `bitty-docs/docs/specifications/performance-budget-rfc.md` PB-6:
//! ≥ 40 MB/s sustained VT parse-and-render on single core of slowest
//! Tier 1 reference machine, fixed synthetic corpus.
//!
//! This bench isolates the parser (`bitty-vt::Parser::advance`) from state
//! (`State::apply`) and render, so regressions can be triaged. It is
//! headless (no `winit::Window`, no `wgpu::Surface`), bounded
//! (`MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096`), and deterministic:
//! the synthetic corpus is a repeatable byte pattern, chunking invariance
//! is asserted once per corpus.
//!
//! Budget reference: `bitty-docs/docs/specifications/performance-budget-rfc.md#pb-6-throughput-floor`.
//! Tooling entry: `tools/perf/latency` for PB-4 and `tools/perf/startup` for PB-1
//! are separate; this bench covers PB-6 only.
//!
//! Run headlessly:
//! ```text
//! cargo bench -p bitty-perf --bench vt_throughput -- --nocapture
//! cargo bench --no-run   # must compile
//! ```

#![forbid(unsafe_code)]

use std::hint::black_box;
use std::time::Instant;

use bitty_vt::Parser;

/// Maximum corpus bytes per file — 8 KiB, matching `bitty-pty::READ_CHUNK_SIZE`.
/// Matches `tests/compat/harness.rs::MAX_CORPUS_BYTES`.
const MAX_CORPUS_BYTES: usize = 8 * 1024;

/// Bound on decoded actions per corpus — matches harness `MAX_ACTIONS`.
const MAX_ACTIONS: usize = 4096;

/// PB-6 budget floor: ≥ 40 MB/s.
const PB6_THROUGHPUT_MB_S: f64 = 40.0;

/// Synthetic corpus: mix of printable, CSI SGR, DECSET, OSC, and malformed
/// bytes derived from `crates/bitty-vt/seeds/*.bin` families (cursor,
/// SGR, erase, DCS, param stress) without embedding real `tmp/references`.
fn synthetic_corpus(len: usize) -> Vec<u8> {
    // Deterministic pattern: printable run + SGR + cursor addressing + DCS probe.
    let chunk: &[u8] = b"\x1b[31;1mHello \x1b[0m\x1b[2J\x1b[H\x1b[?25h world \x1b[38;2;255;128;0m!\n\x1b]0;Bitty\x07\x1bP0;fake|DCS\x1b\\";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let remain = len - out.len();
        let take = chunk.len().min(remain);
        out.extend_from_slice(&chunk[..take]);
    }
    out
}

/// Parse `bytes` with `Parser::advance` and return action count, bounded.
fn parse_bounded(bytes: &[u8]) -> usize {
    assert!(
        bytes.len() <= MAX_CORPUS_BYTES * 16,
        "corpus bound for bench segment"
    );
    let mut parser = Parser::new();
    let mut count = 0usize;
    parser.advance(bytes, |_a| {
        if count < MAX_ACTIONS {
            count += 1;
        }
    });
    // Determinism check: byte-by-byte re-parse must match.
    let mut parser2 = Parser::new();
    let mut count2 = 0usize;
    for b in bytes.iter().copied() {
        parser2.advance(&[b], |_| {
            if count2 < MAX_ACTIONS {
                count2 += 1;
            }
        });
    }
    assert_eq!(count, count2, "deterministic divergence");
    count
}

fn bench_once(corpus_len: usize, iters: usize) -> (f64, usize) {
    let corpus = synthetic_corpus(corpus_len);
    let total_bytes = corpus.len() * iters;
    let start = Instant::now();
    let mut total_actions = 0usize;
    for _ in 0..iters {
        total_actions += black_box(parse_bounded(black_box(&corpus)));
    }
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64().max(1e-9);
    let mb_s = total_bytes as f64 / (1024.0 * 1024.0) / secs;
    (mb_s, total_actions)
}

fn main() {
    // Warmup once (JIT-like caches, allocator).
    let _ = bench_once(8 * 1024, 10);

    let cases = [
        ("tiny_512B", 512usize, 5000usize),
        ("small_8KiB", 8 * 1024, 2000usize),
        ("mid_32KiB", 32 * 1024, 800usize),
    ];

    println!(
        "vt_throughput — PB-6 throughput floor ≥ {PB6_THROUGHPUT_MB_S:.0} MB/s (single core, synthetic corpus, headless)"
    );
    println!(
        "bound: MAX_CORPUS_BYTES={MAX_CORPUS_BYTES}, MAX_ACTIONS={MAX_ACTIONS}, BUDGET=performance-budget-rfc.md#pb-6"
    );
    for (label, corpus_len, iters) in cases {
        let (mb_s, actions) = bench_once(corpus_len, iters);
        let status = if mb_s >= PB6_THROUGHPUT_MB_S {
            "PASS"
        } else {
            "BELOW_BUDGET"
        };
        println!(
            "{label}: corpus={corpus_len} B iters={iters} actions≈{actions} throughput={mb_s:.2} MB/s [{status}]"
        );
        // Soft gate: do not hard-fail CI before reference hardware is defined
        // (RFC cross-cutting rule: harnesses must be defined before budgets become hard gates).
        if mb_s < PB6_THROUGHPUT_MB_S {
            eprintln!(
                "warning: {label} throughput {mb_s:.2} MB/s < PB-6 floor {PB6_THROUGHPUT_MB_S:.0} MB/s — arch constraint, not CI gate (see performance-budget-rfc.md Open items)"
            );
        }
    }
}
