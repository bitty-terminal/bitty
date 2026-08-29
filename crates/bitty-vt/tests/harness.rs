#![forbid(unsafe_code)]
//! Proxy that wires the workspace `tests/compat/harness.rs` corpus into
//! `cargo test -p bitty-vt --test harness_proxy`.
//! The canonical harness lives at `tests/compat/harness.rs` (forbid unsafe,
//! bounded, deterministic). This proxy reuses the same corpora via
//! `CARGO_MANIFEST_DIR`-anchored discovery but adds zero `winit`/`wgpu`
//! deps — it only depends on `bitty-vt` (and `bitty-term-state` via dev?)
//! Instead we stay inside `bitty-vt` and assert `Parser` chunking identity
//! only, keeping the full `State` check in `bitty-compat-lab::harness`.

use std::path::PathBuf;

const MAX_CORPUS_BYTES: usize = 8 * 1024;
const MAX_ACTIONS: usize = 4096;
const MAX_CORPORA_PER_CATEGORY: usize = 64;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}
fn corpus_dir(category: &str) -> PathBuf {
    workspace_root()
        .join("tests/compat")
        .join(category)
        .join("corpus")
}
fn list_corpus(category: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(category);
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
        if out.len() >= MAX_CORPORA_PER_CATEGORY {
            break;
        }
        out.push(p);
    }
    out.sort();
    out
}
const CATS: &[&str] = &[
    "vt", "osc", "keyboard", "mouse", "resize", "unicode", "shell", "tui",
];

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
fn vt_corpus_bounded_and_deterministic_for_bitty_vt() {
    let mut total = 0usize;
    for &cat in CATS {
        for p in list_corpus(cat) {
            let b = std::fs::read(&p).unwrap();
            assert!(b.len() <= MAX_CORPUS_BYTES, "{p:?} > MAX_CORPUS_BYTES");
            let a = parse_twice(&b);
            assert!(a.len() <= MAX_ACTIONS);
            total += 1;
        }
    }
    assert!(total >= 16, "expected >=16 corpora, saw {total}");
}
