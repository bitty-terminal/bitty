//! Compatibility lab harness — headless, bounded, `forbid(unsafe)`.
//!
//! Phase C scaffold for `tests/compat/{vt,osc,keyboard,mouse,resize,unicode,shell,tui}/`.
//!
//! - `#![forbid(unsafe_code)]` — no `unsafe` in this harness or corpora.
//! - Headless — no window, no GPU, no display: `Parser -> TerminalAction -> State`
//!   only, via `bitty-vt` and `bitty-term-state`. No `winit`, `wgpu`, or `Surface`.
//! - Bounded — every input and decoded resource is capped (see constants below);
//!   exceeding a limit yields truncation/inert action, never growth.
//! - No window/GPU leak — this module never constructs `winit::Window`,
//!   `wgpu::Surface`, or `HeadlessRasterizer`; rendering is out of scope for the
//!   compat lab (tested separately in `bitty-render` headless fixtures).
//!
//! References:
//! - `vttest` (Thomas Dickey) — VT100/VT220/VT420 conformance suite; corpora under
//!   `tests/compat/vt/corpus/vttest/` are captured `script` logs or curated
//!   escape sequences from `vttest` menus 1–12. Compare `bitty-vt` actions to
//!   `vttest` expected cell/mode outcomes.
//! - Ghostty / kitty / WezTerm differential — capture the same byte stream fed to
//!   each reference terminal (via `script --timing` or `expect` replay) and diff
//!   `bitty-term-state` `Snapshot` (text + attrs + cursor + modes) against the
//!   reference grid dump (`kitty --dump-commands`, Ghostty `xterm` dump, WezTerm
//!   `wezterm record`). Differential is snapshot-to-snapshot, not pixel.
//! - Existing `bitty-vt` tests — `crates/bitty-vt/tests/replay.rs` fixtures
//!   (`shell_session`, `escape_storm`, `fullscreen_app`, `osc_sweep`) and
//!   `crates/bitty-vt/seeds/*.bin` (14 seeds) are the baseline corpus; this lab
//!   extends them with `tests/compat/*` without forking their guarantees.
//!
//! Usage (headless, deterministic):
//! ```ignore
//! use bitty_vt::Parser;
//! use bitty_term_state::State;
//! let bytes = std::fs::read("tests/compat/vt/corpus/01-cursor.bin").unwrap();
//! assert!(bytes.len() <= compat_harness::MAX_CORPUS_BYTES);
//! let actions = compat_harness::parse_bounded(&bytes);
//! let snapshot = compat_harness::actions_to_snapshot(&actions);
//! // differential: compare `snapshot.text()` to reference dump
//! ```

#![forbid(unsafe_code)]

/// Maximum corpus bytes per file — 8 KiB, matching `bitty-pty::READ_CHUNK_SIZE`.
/// Larger corpora must be split; harness asserts this bound.
pub const MAX_CORPUS_BYTES: usize = 8 * 1024;

/// Maximum decoded actions per corpus — prevents unbounded `Vec` growth.
pub const MAX_ACTIONS: usize = 4096;

/// Maximum OSC payload bytes (mirrors `BoundedString::MAX_LEN`).
pub const MAX_OSC_BYTES: usize = 1024;

/// Maximum corpora per category exercised in one `cargo test` shard.
pub const MAX_CORPORA_PER_CATEGORY: usize = 64;

/// Parse `bytes` to `TerminalAction` stream with deterministic chunking.
///
/// Bounded: asserts `bytes.len() <= MAX_CORPUS_BYTES` and `actions.len() <= MAX_ACTIONS`.
/// Headless: no I/O, no window, no GPU.
pub fn parse_bounded(bytes: &[u8]) -> Vec<bitty_vt::TerminalAction> {
    assert!(
        bytes.len() <= MAX_CORPUS_BYTES,
        "corpus exceeds MAX_CORPUS_BYTES: {} > {}",
        bytes.len(),
        MAX_CORPUS_BYTES
    );
    let mut parser = bitty_vt::Parser::new();
    let mut actions = Vec::new();
    parser.advance(bytes, |action| {
        if actions.len() < MAX_ACTIONS {
            actions.push(action);
        }
    });
    assert!(
        actions.len() <= MAX_ACTIONS,
        "actions exceed MAX_ACTIONS: {}",
        actions.len()
    );
    // Determinism check: re-parse byte-by-byte must yield identical actions.
    let mut parser2 = bitty_vt::Parser::new();
    let mut actions2 = Vec::new();
    for byte in bytes.iter().copied() {
        parser2.advance(&[byte], |a| {
            if actions2.len() < MAX_ACTIONS {
                actions2.push(a);
            }
        });
    }
    assert_eq!(actions, actions2, "deterministic replay diverged");
    actions
}

/// Feed `actions` into a fresh `State` and return its `Snapshot`.
///
/// Bounded via `State` invariants (`GRID_COLUMNS`/`GRID_ROWS`, `SCROLLBACK_MAX_LINES`,
/// `REPLY_CAP_BYTES`, etc.). Headless — no platform or render.
pub fn actions_to_snapshot(actions: &[bitty_vt::TerminalAction]) -> bitty_term_state::Snapshot {
    let mut state = bitty_term_state::State::new();
    for action in actions {
        state.apply(action);
    }
    state.snapshot()
}

/// Differential helper: compare two snapshots text-wise.
///
/// Returns `None` if equal, `Some(diff)` otherwise. Used to compare
/// `bitty-term-state` snapshot against Ghostty/kitty/WezTerm reference dumps
/// (captured offline, normalized to `Snapshot` text).
pub fn diff_snapshots(ours: &bitty_term_state::Snapshot, reference_text: &str) -> Option<String> {
    let ours_text = snapshot_to_text(ours);
    if ours_text == reference_text {
        None
    } else {
        Some(format!(
            "snapshot mismatch: ours len {} vs ref len {}\nours:\n{}\nref:\n{}",
            ours_text.len(),
            reference_text.len(),
            ours_text,
            reference_text
        ))
    }
}

fn snapshot_to_text(snapshot: &bitty_term_state::Snapshot) -> String {
    let mut out = String::new();
    for row in 0..snapshot.height {
        for col in 0..snapshot.width {
            let idx = row * snapshot.width + col;
            let cell = &snapshot.cells[idx];
            // `spacer` is trailing half of wide char — skip to avoid double count
            if cell.spacer {
                continue;
            }
            out.push(cell.glyph);
        }
        if row + 1 < snapshot.height {
            out.push('\n');
        }
    }
    out
}

/// Corpus discovery — bounded, headless directory walk.
///
/// Lists `*.bin` and `*.txt` under `tests/compat/<category>/corpus/` up to
/// `MAX_CORPORA_PER_CATEGORY`. No recursion beyond `corpus/`, no symlink follow.
pub fn list_corpus(category: &str) -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new("tests/compat")
        .join(category)
        .join("corpus");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "bin" && ext != "txt" && ext != "log" {
            continue;
        }
        if out.len() >= MAX_CORPORA_PER_CATEGORY {
            break;
        }
        out.push(path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_is_headless_and_bounded() {
        let bytes = b"\x1b[31mhello\x1b[0m";
        let actions = parse_bounded(bytes);
        assert!(!actions.is_empty());
        let snapshot = actions_to_snapshot(&actions);
        assert_eq!(snapshot.width, bitty_term_state::GRID_COLUMNS);
        assert_eq!(snapshot.height, bitty_term_state::GRID_ROWS);
    }

    #[test]
    fn corpus_bound_enforced() {
        let oversized = vec![b'x'; MAX_CORPUS_BYTES + 1];
        let result = std::panic::catch_unwind(|| parse_bounded(&oversized));
        assert!(result.is_err(), "expected bound assert");
    }
}
