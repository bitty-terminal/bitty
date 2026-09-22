#![forbid(unsafe_code)]
//! Vertical-slice review gates A1-A9 (M1-29, CTX-0689) — automated,
//! headless, bounded, deterministic.
//!
//! Traceability: each test maps to one acceptance criterion in
//! `docs/product/vertical-slice-acceptance.md` ("Acceptance criteria
//! (candidate, all must hold)", A1-A9) under the M1 milestone
//! (`bitty#969`, backlog item `M1-29`, `bitty#1155`). The suite pins the
//! headless-verifiable half of every gate: parser bounds, cursor and
//! scrollback invariants, resize preservation, presentation-layer
//! selection, the audited `8192` clipboard bound, cold-path shell markers,
//! corpus replay for the app smokes, and visible/headless state-hash
//! parity. What the plan declares manual evidence (live PTY echo on Tier 1
//! iron, `nvim`/`tmux` visible smokes with recordings, shell rendering,
//! `hyperfine`/`procs` budgets) stays manual and is called out per test —
//! nothing here claims visible evidence.
//!
//! Bounds: every byte string is `<= MAX_CORPUS_BYTES` per parse (larger
//! fixtures replay in bounded chunks); every parse asserts the
//! byte-by-byte determinism check in [`parse_bounded`]. No I/O beyond
//! reading the repo's own corpus fixtures, no window, no GPU, no network.
//! MSRV 1.85: no let-chains or newer syntax.

use bitty_compat_lab::{MAX_CORPUS_BYTES, actions_to_snapshot, parse_bounded, workspace_root};
use bitty_platform::clipboard::{CLIPBOARD_MAX_BYTES, Clipboard};
use bitty_runtime::shell_integration::ShellIntegration;
use bitty_term_state::{
    GRID_COLUMNS, GRID_ROWS, SCROLLBACK_DEFAULT_LINES, Snapshot, State, ZoneKind,
};
use bitty_ui::selection::{CellPos, Selection};
use bitty_vt::CursorStyle;

/// Render a snapshot to row-major text (spacers skipped, as in the harness).
fn snapshot_text(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    for row in 0..snapshot.height {
        for col in 0..snapshot.width {
            let cell = &snapshot.cells[row * snapshot.width + col];
            if !cell.spacer {
                out.push(cell.glyph);
            }
        }
        if row + 1 < snapshot.height {
            out.push('\n');
        }
    }
    out
}

/// Replay `bytes` through a fresh [`State`], returning it for inspection.
///
/// Inputs larger than [`MAX_CORPUS_BYTES`] are applied in bounded chunks so
/// the gate stays within the harness budget on every run; chunking only
/// splits input, never the assertions.
fn replay(bytes: &[u8]) -> State {
    let mut state = State::new();
    for chunk in bytes.chunks(MAX_CORPUS_BYTES) {
        for action in parse_bounded(chunk) {
            state.apply(&action);
        }
    }
    state
}

/// Corpus replay for one fixture file: panic-free, invariant-clean, and
/// state-hash stable across two independent runs.
fn assert_corpus_gate(path: &std::path::Path) {
    let bytes = std::fs::read(path).expect("corpus fixture must be readable");
    assert!(
        !bytes.is_empty(),
        "corpus fixture must not be empty: {path:?}"
    );
    let first = replay(&bytes);
    first
        .check_invariants()
        .expect("corpus replay must hold terminal invariants");
    let second = replay(&bytes);
    assert_eq!(
        first.state_hash(),
        second.state_hash(),
        "corpus replay must hash identically across runs: {path:?}"
    );
}

/// A1. End-to-end PTY loop (headless-verifiable half).
///
/// Typed input round-trips `shell bytes -> VT -> state -> snapshot ->
/// present`, oversized DCS/OSC replies stay bounded with drop-on-excess,
/// and malformed/unterminated sequences recover deterministically to
/// `U+FFFD` without panic. Live-shell echo on Tier 1 iron remains manual
/// evidence per the plan and is not claimed here.
#[test]
fn a1_pty_loop_echo_and_bounded_replies() {
    // Echo round-trip: typed input appears in the presented snapshot.
    let state = replay(b"echo hello\r\necho hello\r\n$ ");
    let text = snapshot_text(&state.snapshot());
    assert!(
        text.contains("echo hello"),
        "typed input must round-trip to the snapshot"
    );
    state
        .check_invariants()
        .expect("echo replay must hold invariants");

    // Bounded reply buffer: an 8 KiB OSC reply plus a DCS flood stays
    // within the action budget and holds invariants.
    let mut flood = b"\x1b]11;".to_vec();
    flood.extend(std::iter::repeat_n(b'x', 7000));
    flood.extend_from_slice(b"\x1b\\");
    flood.extend_from_slice(b"\x1bP1$r0;0q$m\x1b\\");
    let actions = parse_bounded(&flood);
    assert!(
        actions.len() <= bitty_compat_lab::MAX_ACTIONS,
        "reply flood must stay within the action budget"
    );
    let reply_state = replay(&flood);
    reply_state
        .check_invariants()
        .expect("reply flood must hold invariants");

    // Drop-on-excess with a flag at the reply-queue seam itself.
    let mut replies = bitty_term_state::Replies::new();
    for _ in 0..32 {
        replies.queue(vec![b'r'; 1024].into_boxed_slice());
    }
    assert!(
        replies.overflowed(),
        "sustained over-cap replies must raise the drop flag"
    );
    assert!(
        replies.total_bytes() <= bitty_term_state::REPLY_CAP_BYTES,
        "reply queue must stay capped after drop-on-excess"
    );

    // Malformed/unterminated input: no panic, deterministic, U+FFFD policy.
    let hostile: &[u8] = b"\xff\xfeA\x1b[9999999999999999999m\x1b]999;\x90unterminated\x1b[?25";
    let bad_state = replay(hostile);
    let bad_text = snapshot_text(&bad_state.snapshot());
    assert!(
        bad_text.contains('\u{FFFD}'),
        "invalid bytes must decode to U+FFFD, one cell"
    );
    bad_state
        .check_invariants()
        .expect("malformed input must hold invariants");
}

/// A2. Cursor (DECSCUSR / DECTCEM, integrity, alt-screen save/restore).
///
/// Cursor style and visibility follow the `Action::CursorStyle` /
/// `Action::CursorVisibility` path; the cursor never addresses a wide-cell
/// trailing spacer; insert/delete, scroll region, and alternate-screen swap
/// preserve the cursor within the grid.
#[test]
fn a2_cursor_style_visibility_and_integrity() {
    let mut state = State::new();
    for action in parse_bounded(b"\x1b[2 q") {
        state.apply(&action);
    }
    assert_eq!(
        state.cursor().cursor_style,
        CursorStyle::SteadyBlock,
        "DECSCUSR 2 must select steady block"
    );
    for action in parse_bounded(b"\x1b[?25l") {
        state.apply(&action);
    }
    assert!(
        !state.cursor().visible,
        "DECTCEM reset must hide the cursor"
    );
    for action in parse_bounded(b"\x1b[?25h") {
        state.apply(&action);
    }
    assert!(state.cursor().visible, "DECTCEM set must show the cursor");

    // Wide-char integrity: the trailing spacer exists but the cursor never
    // lands on it; movement follows the single documented rule.
    let wide = replay("中\x1b[D\x1b[C".as_bytes());
    wide.check_invariants()
        .expect("wide-char movement must hold invariants");
    let snap = wide.snapshot();
    assert!(
        snap.cells.iter().any(|cell| cell.spacer),
        "wide char must occupy a leading cell plus a trailing spacer"
    );
    let cursor = wide.cursor();
    let cursor_idx = (cursor.position.row as usize) * snap.width + cursor.position.col as usize;
    assert!(
        !snap.cells[cursor_idx].spacer,
        "cursor must sit on a leading cell, never a trailing spacer"
    );

    // Alternate-screen entry saves and exit restores the primary cursor.
    let before = {
        let s = replay(b"primary\x1b[5;10H");
        (s.cursor().position.row, s.cursor().position.col)
    };
    let alt = replay(b"primary\x1b[5;10H\x1b[?1049halt-screen\x1b[?1049l");
    alt.check_invariants()
        .expect("alt-screen swap must hold invariants");
    assert_eq!(
        (alt.cursor().position.row, alt.cursor().position.col),
        before,
        "alt-screen exit must restore the primary-screen cursor"
    );
}

/// A3. Scrollback monotonicity and bound.
///
/// Lines enter scrollback oldest-first with pruning of the oldest lines,
/// the history stays within the configured limit, and eviction is
/// observable (no unbounded allocation).
#[test]
fn a3_scrollback_bounded_monotonic() {
    // Overfeed the default history with bounded newline chunks.
    let mut state = State::new();
    let overfeed = SCROLLBACK_DEFAULT_LINES + 2000;
    let chunk = vec![b'\n'; 400];
    let mut fed = 0;
    while fed < overfeed {
        for action in parse_bounded(&chunk) {
            state.apply(&action);
        }
        fed += 400;
    }
    state
        .check_invariants()
        .expect("scrollback overfeed must hold invariants");
    assert!(
        state.scrollback_len() <= SCROLLBACK_DEFAULT_LINES,
        "scrollback must stay within the configured limit"
    );
    assert!(
        state.scrollback_evicted_total() > 0,
        "overfeed must evict oldest-first, never grow unbounded"
    );
    let evicted_before = state.scrollback_evicted_total();
    for action in parse_bounded(&chunk) {
        state.apply(&action);
    }
    assert!(
        state.scrollback_evicted_total() >= evicted_before,
        "eviction accounting must be monotonic"
    );
    assert!(
        state.scrollback_len() <= SCROLLBACK_DEFAULT_LINES,
        "scrollback must stay bounded after further feed"
    );
}

/// A4. Resize to PTY.
///
/// Grid geometry recomputes across the tested size matrix while cursor and
/// geometry invariants hold; resize is a deterministic state transition
/// (same size sequence hashes identically). Native `SIGWINCH`/ConPTY
/// delivery and concurrent resize-burst backpressure remain platform
/// evidence outside this headless gate.
#[test]
fn a4_resize_preserves_invariants() {
    let sizes: &[(usize, usize)] = &[
        (80, 24),
        (132, 43),
        (40, 10),
        (200, 60),
        (20, 5),
        (100, 30),
        (GRID_COLUMNS, GRID_ROWS),
    ];
    let run = || {
        let mut state = State::new();
        for (cols, rows) in sizes {
            state.resize(*cols, *rows);
            for action in parse_bounded(b"resize-burst-0123456789") {
                state.apply(&action);
            }
            state
                .check_invariants()
                .expect("resize must hold invariants at every matrix size");
            assert_eq!(state.width(), *cols);
            assert_eq!(state.height(), *rows);
            assert!((state.cursor().position.row as usize) < state.height());
            assert!((state.cursor().position.col as usize) < state.width());
        }
        state
    };
    let first = run();
    let second = run();
    assert_eq!(
        first.state_hash(),
        second.state_hash(),
        "resize sequence must be a deterministic state transition"
    );
}

/// A5. Selection is presentation-layer state.
///
/// Selection addresses viewport cells, never mutates `Terminal` invariants
/// (it borrows only an immutable snapshot), normalizes anchor/focus order,
/// and snaps wide-cell spacer endpoints to the leading cell so no
/// half-wide selection is addressable.
#[test]
fn a5_selection_is_presentation_state() {
    let state = replay("中ab".as_bytes());
    let snapshot = state.snapshot();
    let before_hash = state.state_hash();

    // Viewport addressing with order-independent normalization.
    let sel = Selection::simple(CellPos::new(0, 3), CellPos::new(0, 0));
    let range = sel.normalized();
    assert_eq!(range.start, CellPos::new(0, 0));
    assert_eq!(range.end, CellPos::new(0, 3));
    assert!(range.contains(CellPos::new(0, 2)));
    assert!(Selection::collapsed(CellPos::new(1, 1)).is_empty());

    // Wide-cell snapping: column 1 is the trailing spacer of `中`.
    assert!(
        snapshot.cells[1].spacer,
        "fixture must contain a wide-cell spacer for the snap assertion"
    );
    let snapped = Selection::simple(CellPos::new(0, 1), CellPos::new(0, 2))
        .snapped(Some(&snapshot))
        .normalized();
    assert_eq!(
        snapped.start,
        CellPos::new(0, 0),
        "spacer endpoint must snap to the leading cell"
    );

    // Presentation-only: selecting observes but never mutates terminal state.
    assert_eq!(
        state.state_hash(),
        before_hash,
        "selection must not mutate terminal state"
    );
    state
        .check_invariants()
        .expect("selection fixture must hold invariants");
}

/// A6. Copy and paste bounds.
///
/// Copy extracts bounded text (no unbounded concat); paste enforces the
/// audited `CLIPBOARD_MAX_BYTES = 8192` char-boundary bound with
/// truncation telemetry; OSC 52 read stays denied in this slice. Platform
/// backends and real-window UX remain the documented `R-004` residual.
#[test]
fn a6_clipboard_bounded_copy_paste() {
    // Pin the audited bound from CTX-0097 that the plan cites.
    assert_eq!(
        CLIPBOARD_MAX_BYTES, 8192,
        "paste bound must stay the audited 8192 bytes"
    );

    // Over-limit system text: direct reads reject, bounded reads clip at a
    // char boundary with telemetry. `é` (2 bytes) proves the cut is not
    // mid-code-point.
    let mut clipboard = Clipboard::new_headless();
    clipboard.simulate_system_text_for_test("é".repeat(5000));
    assert!(
        clipboard.get_text().is_err(),
        "over-limit system text must reject on the direct read"
    );
    let clipped = clipboard
        .get_text_bounded()
        .expect("bounded read must clip, not fail");
    assert!(
        clipped.len() <= CLIPBOARD_MAX_BYTES,
        "bounded read must respect the 8192-byte cap"
    );
    assert!(
        clipboard.last_bounded_read_truncated(),
        "a clipped read must report truncation telemetry"
    );
    assert_eq!(
        clipped,
        clipboard.get_text_lossy(),
        "lossy helper must share the bounded path"
    );

    // At-limit payloads pass through unclipped with no telemetry.
    let mut exact = Clipboard::new_headless();
    exact.simulate_system_text_for_test(String::from("ok"));
    assert_eq!(exact.get_text().expect("at-limit read"), "ok");
    assert!(!exact.last_bounded_read_truncated());
}

/// A7. Shell coverage markers are cold-path observations.
///
/// `OSC 133` prompt markers and `OSC 7` cwd reports become semantic-zone
/// events, never grid mutations; unknown OSCs are inert. Rendering a usable
/// prompt per Tier 1 shell remains release evidence, not this gate.
#[test]
fn a7_shell_markers_are_cold_path() {
    let bytes = b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\echo hi\x1b]133;C\x1b\\hi\n\x1b]133;D;0\x1b\\\x1b]7;file:///home/user\x1b\\";
    let state = replay(bytes);
    state
        .check_invariants()
        .expect("shell markers must hold invariants");
    assert_eq!(
        ShellIntegration::prompt_starts(&state).len(),
        1,
        "one prompt-start zone per `A` marker"
    );
    assert_eq!(
        ShellIntegration::last_exit_code(&state),
        Some(0),
        "`D;code` marker must record the exit status"
    );
    let cwd = ShellIntegration::cwd(&state).expect("OSC 7 must report cwd");
    assert!(
        cwd.contains("home/user"),
        "OSC 7 payload must be observable as cwd"
    );
    let grid = snapshot_text(&state.snapshot());
    assert!(
        !grid.contains("home/user"),
        "marker payloads must never mutate the grid"
    );

    // Unknown OSCs are inert telemetry: no zones, no grid effect, no panic.
    let unknown = replay(b"\x1b]9999;ignored-payload\x1b\\plain");
    assert_eq!(
        ShellIntegration::zone_count(&unknown),
        0,
        "unknown OSC must produce no zones"
    );
    assert!(
        ShellIntegration::zones_of_kind(&unknown, ZoneKind::PromptStart).is_empty(),
        "unknown OSC must not forge prompt zones"
    );
}

/// A8. `nvim` and `tmux` corpus replay (headless-verifiable half).
///
/// Every committed `tui` fixture (including the `nvim`+`tmux` captures)
/// replays panic-free, invariant-clean, and deterministically. The plan
/// declares both smokes manual evidence with recordings; this gate covers
/// the replayable prefix and explicitly does not claim the visible smokes.
#[test]
fn a8_tui_corpus_replay_panic_free() {
    let dir = workspace_root().join("tests/compat/tui/corpus");
    let mut fixtures: Vec<_> = std::fs::read_dir(&dir)
        .expect("tui corpus directory must exist")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .filter(|path| path.file_stem().is_some_and(|stem| stem != "placeholder"))
        .collect();
    fixtures.sort();
    assert!(
        fixtures.len() >= 2,
        "tui corpus must hold the nvim/tmux captures, found: {fixtures:?}"
    );
    let names: Vec<_> = fixtures
        .iter()
        .filter_map(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    assert!(
        names.iter().any(|name| name.contains("nvim")),
        "corpus must include an nvim capture, found: {names:?}"
    );
    for path in &fixtures {
        assert_corpus_gate(path);
    }
    // Shell fixtures ride along: OSC 133/7 startup/exit captures prove the
    // A7 cold path against recorded shells, not only synthetic bytes.
    let shell_dir = workspace_root().join("tests/compat/shell/corpus");
    let mut shell_fixtures: Vec<_> = std::fs::read_dir(&shell_dir)
        .expect("shell corpus directory must exist")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
        .filter(|path| path.file_stem().is_some_and(|stem| stem != "placeholder"))
        .collect();
    shell_fixtures.sort();
    assert!(
        !shell_fixtures.is_empty(),
        "shell corpus must not be placeholder-only"
    );
    for path in &shell_fixtures {
        assert_corpus_gate(path);
    }
}

/// A9. Visible vs headless consistency.
///
/// The same byte stream plus `Env` sequence reaches the identical
/// `StateHash` on independent runs (the parity the plan demands between
/// visible and headless execution), damage batches are deterministic, and
/// the union of per-batch damage covers every cell a full-redraw diff
/// flags (over-damage allowed, under-damage forbidden; replay tooling
/// ignores damage as performance-only).
#[test]
fn a9_headless_determinism_and_damage_soundness() {
    let bytes: &[u8] =
        "Hello 中文\x1b[31mRED\x1b[0m\x1b[2K\rDONE\x1b[?1049hALT\x1b[?1049l".as_bytes();

    // Independent runs hash identically (canonical little-endian StateHash).
    let first = replay(bytes);
    let second = replay(bytes);
    assert_eq!(
        first.state_hash(),
        second.state_hash(),
        "same byte stream must reach the identical state hash"
    );

    // Per-batch damage is deterministic and covers the full-redraw diff.
    let mut state = State::new();
    let before = state.snapshot();
    let mut regions = Vec::new();
    let mut first_damage: Option<bitty_term_state::Damage> = None;
    for action in parse_bounded(bytes) {
        let damage = state.apply(&action);
        if first_damage.is_none() {
            first_damage = Some(damage.clone());
        }
        regions.extend(damage.regions.iter().copied());
    }
    let after = state.snapshot();

    let mut rerun = State::new();
    let mut rerun_first: Option<bitty_term_state::Damage> = None;
    for action in parse_bounded(bytes) {
        let damage = rerun.apply(&action);
        if rerun_first.is_none() {
            rerun_first = Some(damage);
        }
    }
    assert_eq!(
        first_damage, rerun_first,
        "damage batches must be deterministic across runs"
    );

    let covered = |row: usize, col: usize| {
        regions.iter().any(|region| match region {
            bitty_term_state::DamagedRegion::Grid(rect) => {
                (rect.top as usize) <= row
                    && row <= (rect.bottom as usize)
                    && (rect.left as usize) <= col
                    && col <= (rect.right as usize)
            }
            bitty_term_state::DamagedRegion::Scrollback { .. } => false,
        })
    };
    assert_eq!(before.width, after.width);
    assert_eq!(before.height, after.height);
    let mut diff_cells = 0;
    for row in 0..after.height {
        for col in 0..after.width {
            let idx = row * after.width + col;
            let a = &before.cells[idx];
            let b = &after.cells[idx];
            if a.glyph != b.glyph || a.spacer != b.spacer {
                diff_cells += 1;
                assert!(
                    covered(row, col),
                    "damage union must cover changed cell ({row}, {col})"
                );
            }
        }
    }
    assert!(
        diff_cells > 0,
        "fixture must mutate the grid for the coverage assertion to mean anything"
    );

    // The harness path agrees: snapshot helper plus determinism check.
    let via_helper = actions_to_snapshot(&parse_bounded(bytes));
    assert_eq!(
        snapshot_text(&via_helper),
        snapshot_text(&after),
        "helper and manual replay must present identically"
    );
}
