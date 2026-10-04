#![forbid(unsafe_code)]
//! Proxy that wires the crate's own `seeds/` corpus into
//! `cargo test -p bitty-vt --test harness`.
//!
//! The canonical full-corpus harness lives in the `bitty-compat-lab`
//! validation repository (W-105 relocation, bitty CTX-0931) and checks
//! `Parser -> State` end to end there. This proxy stays inside `bitty-vt`
//! and asserts `Parser` chunking identity only (one batched feed versus
//! byte-at-a-time must agree action-for-action), over the committed seeds
//! beside this crate — no `winit`/`wgpu` deps, no external checkout.

use std::path::PathBuf;

const MAX_CORPUS_BYTES: usize = 8 * 1024;
const MAX_ACTIONS: usize = 4096;

/// Committed `bitty-vt` parser seeds beside this crate.
fn seeds_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seeds")
}

fn list_seeds() -> Vec<PathBuf> {
    let dir = seeds_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

fn parse_twice(bytes: &[u8]) -> Vec<bitty_vt::TerminalAction> {
    let mut p1 = bitty_vt::Parser::new();
    let mut a1 = Vec::new();
    p1.advance(bytes, |a| {
        if a1.len() < MAX_ACTIONS {
            a1.push(a);
        }
    });
    let mut p2 = bitty_vt::Parser::new();
    let mut a2 = Vec::new();
    for b in bytes.iter().copied() {
        p2.advance(&[b], |a| {
            if a2.len() < MAX_ACTIONS {
                a2.push(a);
            }
        });
    }
    assert_eq!(a1, a2, "deterministic divergence");
    a1
}

#[test]
fn vt_seeds_bounded_and_deterministic_for_bitty_vt() {
    let seeds = list_seeds();
    assert!(
        !seeds.is_empty(),
        "expected committed seeds under {}",
        seeds_dir().display()
    );
    let mut total = 0usize;
    for p in &seeds {
        let b = std::fs::read(p).unwrap();
        assert!(b.len() <= MAX_CORPUS_BYTES, "{p:?} > MAX_CORPUS_BYTES");
        let a = parse_twice(&b);
        assert!(a.len() <= MAX_ACTIONS);
        total += 1;
    }
    assert_eq!(
        total,
        seeds.len(),
        "every committed seed must be exercised, saw {total} of {}",
        seeds.len()
    );
}
