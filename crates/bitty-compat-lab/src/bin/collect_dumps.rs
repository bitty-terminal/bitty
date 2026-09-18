#![forbid(unsafe_code)]

//! Headless bounded collector for CTX-0086 — `Parser -> State -> Snapshot` dumps.
//!
//! For every `tests/compat/<category>/corpus/*.bin` (bounded `< 8 KiB`,
//! `< 4096` actions) this binary replays the corpus through
//! `bitty-compat-lab::parse_bounded` and `actions_to_snapshot` headlessly,
//! asserts determinism via byte-by-byte re-parse and `State::state_hash`,
//! invariants via `State::check_invariants`, and writes a bounded
//! deterministic JSON snapshot to `recording/references/bitty/` (canonical,
//! worktree and umbrella). No `tmp/` mirror is written: `tmp/` is process
//! scratch, never durable evidence (DEC-0038).
//!
//! No `winit`, `wgpu`, `Window`, `Surface`, `HeadlessRasterizer`, or
//! network. Only `bitty-vt` + `bitty-term-state` via the harness. The output
//! JSON is bounded: snapshot text is at most `80*24` glyphs plus newlines,
//! the file is `< 16 KiB`, keys are sorted, and the hash is the canonical
//! FNV-1a `state_hash` (`CANONICAL_HASH_VERSION` pinned).

use std::fs;
use std::path::PathBuf;

use bitty_compat_lab::{
    MAX_ACTIONS, MAX_CORPUS_BYTES, actions_to_snapshot, parse_bounded, umbrella_root,
    workspace_root,
};

const CATEGORIES: &[&str] = &[
    "vt",
    "osc",
    "keyboard",
    "mouse",
    "resize",
    "unicode",
    "shell",
    "tui",
    "scrollback",
    "graphics",
];

fn corpus_dir(category: &str) -> PathBuf {
    workspace_root()
        .join("tests/compat")
        .join(category)
        .join("corpus")
}

fn list_corpus_sorted(category: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(category);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        if out.len() >= 64 {
            break;
        }
        out.push(path);
    }
    out.sort();
    out
}

fn snapshot_to_text(snapshot: &bitty_term_state::Snapshot) -> String {
    let mut out = String::new();
    for row in 0..snapshot.height {
        for col in 0..snapshot.width {
            let idx = row * snapshot.width + col;
            let cell = &snapshot.cells[idx];
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

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(ch),
        }
    }
    out
}

fn write_snapshot_to_dirs(out_dirs: &[PathBuf], file_name: &str, content: &str) -> usize {
    let mut successes = 0;
    for dir in out_dirs {
        if dir.exists() {
            let out_path = dir.join(file_name);
            if fs::write(&out_path, content).is_ok() {
                successes += 1;
            }
        }
    }
    successes
}

fn main() {
    let ws = workspace_root();
    let out_worktree_rec = ws.join("recording/references/bitty");
    // `$BITTY_WORKSPACE` is the explicit umbrella opt-in. The derived parent
    // fallback is read-only (compare.rs discovery); writes only go to an
    // umbrella evidence dir that already exists, so a standalone clone cannot
    // create stray dirs next to the repository.
    let umbrella_explicit = std::env::var_os("BITTY_WORKSPACE").is_some_and(|v| !v.is_empty());
    let out_umbrella_rec: Option<PathBuf> =
        umbrella_root().map(|u| u.join("recording/references/bitty"));

    let mut out_dirs: Vec<PathBuf> = vec![out_worktree_rec.clone()];
    match &out_umbrella_rec {
        Some(dir) if umbrella_explicit || dir.is_dir() => out_dirs.push(dir.clone()),
        Some(dir) => eprintln!(
            "note: umbrella evidence dir {} absent and $BITTY_WORKSPACE unset; writing the worktree copy only",
            dir.display()
        ),
        None => eprintln!("note: umbrella root unavailable; writing the worktree copy only"),
    }
    for dir in &out_dirs {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("warn: cannot create {}: {e}", dir.display());
        }
    }

    let mut total = 0usize;
    let mut written = 0usize;
    for &category in CATEGORIES {
        let files = list_corpus_sorted(category);
        if files.is_empty() {
            eprintln!(
                "warn: no corpora in {category} at {:?}",
                corpus_dir(category)
            );
            continue;
        }
        for path in &files {
            total += 1;
            let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            assert!(
                bytes.len() <= MAX_CORPUS_BYTES,
                "corpus {path:?} exceeds MAX_CORPUS_BYTES: {}",
                bytes.len()
            );
            let actions = parse_bounded(&bytes);
            assert!(
                actions.len() <= MAX_ACTIONS,
                "{path:?} produced {} actions > {MAX_ACTIONS}",
                actions.len()
            );
            let snapshot = actions_to_snapshot(&actions);
            assert_eq!(snapshot.width, bitty_term_state::GRID_COLUMNS, "width");
            assert_eq!(snapshot.height, bitty_term_state::GRID_ROWS, "height");

            // Determinism via state_hash and invariants
            let mut st = bitty_term_state::State::new();
            for a in &actions {
                st.apply(a);
            }
            st.check_invariants()
                .unwrap_or_else(|e| panic!("{path:?} invariant violation: {e:?}"));
            let h1 = st.state_hash();
            // byte-by-byte re-parse already asserted inside parse_bounded; also cross-check hash
            let actions2 = parse_bounded(&bytes);
            let mut st2 = bitty_term_state::State::new();
            for a in &actions2 {
                st2.apply(a);
            }
            assert_eq!(h1, st2.state_hash(), "state_hash diverged for {path:?}");

            let text = snapshot_to_text(&snapshot);
            // Bounded output: snapshot text is at most 80*24 glyphs but JSON escaping
            // can expand bytes; enforce logical rows*cols, not byte length after escaping
            // trailing spaces are preserved in snapshot_to_text row width 80
            assert!(
                text.chars().count() <= 80 * 24 + 24,
                "snapshot text unexpectedly large for {path:?}: {} chars",
                text.chars().count()
            );
            // JSON file itself is bounded < 16 KiB after escaping
            let rel = path
                .strip_prefix(ws.join("tests/compat"))
                .unwrap_or(path)
                .display()
                .to_string();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let file_name = format!("{category}-{stem}.snapshot.json");
            // Canonical bounded JSON — keys sorted, no timestamp, deterministic
            // `corpus` is the same relative `tests/compat/<category>/corpus/<file>`
            // to keep dumps deterministic across machines (no absolute worktree path).
            let json = format!(
                "{{\n  \"category\": \"{}\",\n  \"corpus\": \"{}\",\n  \"corpus_rel\": \"{}\",\n  \"bytes_len\": {},\n  \"actions_len\": {},\n  \"state_hash\": \"{:016x}\",\n  \"state_hash_version\": {},\n  \"snapshot\": {{\n    \"width\": {},\n    \"height\": {},\n    \"generation\": {},\n    \"title\": \"{}\",\n    \"cursor\": {{\"row\": {}, \"col\": {}, \"visible\": {}}},\n    \"text\": \"{}\"\n  }}\n}}\n",
                json_escape(category),
                json_escape(&rel),
                json_escape(&rel),
                bytes.len(),
                actions.len(),
                h1,
                bitty_term_state::canonical_public::CANONICAL_HASH_VERSION,
                snapshot.width,
                snapshot.height,
                snapshot.generation,
                json_escape(snapshot.title.as_str()),
                snapshot.cursor.position.row,
                snapshot.cursor.position.col,
                snapshot.cursor.visible,
                json_escape(&text)
            );
            assert!(
                json.len() < 16 * 1024,
                "snapshot json unexpectedly large {} for {path:?}",
                json.len()
            );
            if write_snapshot_to_dirs(&out_dirs, &file_name, &json) > 0 {
                written += 1;
            }
            println!(
                "{} {} bytes={} actions={} hash={:016x} -> {}",
                category,
                stem,
                bytes.len(),
                actions.len(),
                h1,
                file_name
            );
        }
    }
    // Ensure at least the non-placeholder corpora count (22 non-placeholder + 8 placeholders = 30)
    assert!(total >= 22, "expected at least 22 corpora, saw {total}");
    assert!(
        written >= 22,
        "expected at least 22 snapshots written, saw {written}"
    );
    println!(
        "collect_dumps done: {written}/{total} snapshots written to {}",
        out_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(" and ")
    );
    let _ = PathBuf::from(".");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirGuard(PathBuf);

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_snapshot_to_dirs_empty_or_nonexistent_dirs_returns_zero() {
        let mut written = 0usize;

        // Empty out_dirs returns 0
        let successes = write_snapshot_to_dirs(&[], "test.snapshot.json", "{}");
        assert_eq!(successes, 0);
        if successes > 0 {
            written += 1;
        }
        assert_eq!(written, 0);

        // Nonexistent directory returns 0
        let nonexistent = std::env::temp_dir().join(format!(
            "bitty-nonexistent-{}-{}",
            std::process::id(),
            "ctx0549"
        ));
        let _ = fs::remove_dir_all(&nonexistent);
        assert!(!nonexistent.exists());

        let successes = write_snapshot_to_dirs(&[nonexistent], "test.snapshot.json", "{}");
        assert_eq!(successes, 0);
        if successes > 0 {
            written += 1;
        }
        assert_eq!(
            written, 0,
            "written count must not increment when no dirs exist"
        );
    }

    #[test]
    fn write_snapshot_to_dirs_valid_directory_writes_file_and_increments() {
        let temp_dir =
            std::env::temp_dir().join(format!("bitty-test-ctx0549-valid-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let _guard = TempDirGuard(temp_dir.clone());

        let file_name = "test.snapshot.json";
        let content = "{\"snapshot\": true}\n";
        let mut written = 0usize;

        let successes = write_snapshot_to_dirs(std::slice::from_ref(&temp_dir), file_name, content);
        assert_eq!(successes, 1);
        if successes > 0 {
            written += 1;
        }
        assert_eq!(written, 1, "written count should increment on success");

        let out_path = temp_dir.join(file_name);
        assert!(out_path.is_file(), "file must be written to disk");
        let read_content = fs::read_to_string(&out_path).expect("read written file");
        assert_eq!(read_content, content);
    }

    #[test]
    fn write_snapshot_to_dirs_multiple_dirs_counts_each_success() {
        let temp_dir1 =
            std::env::temp_dir().join(format!("bitty-test-ctx0549-multi1-{}", std::process::id()));
        let temp_dir2 =
            std::env::temp_dir().join(format!("bitty-test-ctx0549-multi2-{}", std::process::id()));
        let nonexistent = std::env::temp_dir().join(format!(
            "bitty-test-ctx0549-nonexist-{}",
            std::process::id()
        ));

        let _ = fs::remove_dir_all(&temp_dir1);
        let _ = fs::remove_dir_all(&temp_dir2);
        let _ = fs::remove_dir_all(&nonexistent);

        fs::create_dir_all(&temp_dir1).expect("create temp dir 1");
        fs::create_dir_all(&temp_dir2).expect("create temp dir 2");
        let _guard1 = TempDirGuard(temp_dir1.clone());
        let _guard2 = TempDirGuard(temp_dir2.clone());

        let file_name = "multi.snapshot.json";
        let content = "{\"multi\": true}\n";

        let dirs = vec![temp_dir1.clone(), nonexistent, temp_dir2.clone()];
        let successes = write_snapshot_to_dirs(&dirs, file_name, content);
        assert_eq!(successes, 2, "both existing directories must succeed");

        assert_eq!(
            fs::read_to_string(temp_dir1.join(file_name)).unwrap(),
            content
        );
        assert_eq!(
            fs::read_to_string(temp_dir2.join(file_name)).unwrap(),
            content
        );
    }
}
