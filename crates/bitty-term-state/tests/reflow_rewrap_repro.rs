//! CTX-0266 reflow: narrowing rewraps (no tail loss), widening unwraps.
//! Headless, deterministic, no I/O. Covers the task acceptance set:
//! 80->40->80 roundtrip, wide CJK + combining atomicity, scrollback
//! coherence, and alt-screen no-reflow (xterm).

#![forbid(unsafe_code)]

use bitty_term_state::{Mode, State, TerminalAction};
use bitty_vt::{ControlChar, GraphemeCell};

fn prints(state: &mut State, text: &str) {
    for c in text.chars() {
        state.apply(&TerminalAction::Print(GraphemeCell::from(c)));
    }
}

/// Collects all addressable text: scrollback lines oldest-first plus live grid rows,
/// each row trimmed of trailing blanks, spacers skipped, combining marks kept with base.
fn addressable_text(state: &State) -> String {
    let mut out = String::new();
    for line in state.scrollback() {
        let mut row = String::new();
        for cell in line.cells.iter() {
            if cell.spacer {
                continue;
            }
            row.push(cell.glyph);
            for m in cell.zerowidth.iter() {
                row.push(*m);
            }
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    let snap = state.snapshot();
    for r in 0..snap.height {
        let mut row = String::new();
        for c in 0..snap.width {
            let cell = &snap.cells[r * snap.width + c];
            if cell.spacer {
                continue;
            }
            row.push(cell.glyph);
            for m in cell.zerowidth.iter() {
                row.push(*m);
            }
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

#[test]
fn narrow_rewraps_instead_of_truncating_and_wide_unwraps_back() {
    let mut s = State::new();
    assert_eq!((s.width(), s.height()), (80, 24));
    // Fill row 0 with 80 distinct addressable chars (no newline).
    let original: String = (0..80)
        .map(|i| (b'0'..=b'9').cycle().nth(i).unwrap() as char)
        .collect();
    // Use printable distinct pattern: 0-9 repeating gives 80 chars.
    prints(&mut s, &original);
    assert!(s.check_invariants().is_ok());

    // Narrow to 40: all 80 chars must remain addressable across scrollback+grid.
    s.resize(40, 24);
    assert_eq!(s.width(), 40);
    assert!(s.check_invariants().is_ok());
    let text = addressable_text(&s);
    let condensed: String = text.chars().filter(|c| *c != '\n' && *c != ' ').collect();
    for ch in original.chars() {
        assert!(
            condensed.contains(ch),
            "narrowed content lost tail: missing {ch:?} in {condensed:?}"
        );
    }
    // Stronger: the 80-char sequence must appear in order (rewrapped, not truncated).
    let flat: String = text
        .chars()
        .filter(|c| *c != '\n')
        .collect::<String>()
        .replace(' ', "");
    assert!(
        flat.contains(&original),
        "rewrapped rows must preserve full 80-char line in order, got {flat:?}"
    );

    // Widen back to 80: must unwrap to a single 80-col line.
    s.resize(80, 24);
    assert_eq!(s.width(), 80);
    assert!(s.check_invariants().is_ok());
    let text2 = addressable_text(&s);
    let flat2: String = text2
        .chars()
        .filter(|c| *c != '\n')
        .collect::<String>()
        .replace(' ', "");
    assert!(
        flat2.contains(&original),
        "widen must restore single 80-char line, got {flat2:?}"
    );
    // The unwrapped line should occupy one row (first non-empty row equals original).
    let rows: Vec<String> = text2.lines().map(|l| l.to_string()).collect();
    let first_full = rows.iter().find(|r| r.len() >= 80);
    assert!(
        first_full.is_some_and(|r| r.contains(&original)),
        "expected one row containing the full line after unwrap, got {rows:?}"
    );
}

#[test]
fn wide_and_combining_never_split_mid_grapheme() {
    let mut s = State::new();
    // 40 CJK wides fill exactly 80 cols (one full row, soft state clean).
    let wides: String = "中".repeat(40);
    prints(&mut s, &wides);
    // Combining mark rides on the last wide's lead (CR-TERM-01).
    prints(&mut s, "́");
    assert!(s.check_invariants().is_ok());

    // Narrow to 40 cols: each row holds 20 wides. 40 wides -> 2 rows.
    s.resize(40, 24);
    assert!(s.check_invariants().is_ok());
    // No orphan halves anywhere (grid + scrollback).
    let snap = s.snapshot();
    for r in 0..snap.height {
        for c in 0..snap.width {
            let cell = &snap.cells[r * snap.width + c];
            if cell.spacer {
                assert!(c > 0, "spacer at col 0 row {r}");
                let lead = &snap.cells[r * snap.width + c - 1];
                assert_eq!(lead.width, 2, "spacer without wide lead at {r}:{c}");
                assert!(!lead.spacer);
            } else if cell.width == 2 {
                assert!(c + 1 < snap.width, "wide lead at right margin row {r}");
                assert!(snap.cells[r * snap.width + c + 1].spacer);
            }
        }
    }
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 40);
        for (i, cell) in line.cells.iter().enumerate() {
            if cell.spacer {
                assert!(i > 0 && line.cells[i - 1].width == 2);
            } else if cell.width == 2 {
                assert!(i + 1 < line.cells.len() && line.cells[i + 1].spacer);
            }
        }
    }
    // All 40 wides still addressable, combining mark kept with its base.
    let text = addressable_text(&s);
    let wide_count = text.chars().filter(|c| *c == '中').count();
    assert_eq!(wide_count, 40, "must keep all wides, got {text:?}");
    assert!(
        text.contains("́"),
        "combining mark must survive reflow with its base"
    );
    // The mark must sit on a wide lead, never on a spacer.
    let mut mark_on_lead = false;
    for line in s.scrollback() {
        for cell in line.cells.iter() {
            if !cell.spacer && cell.glyph == '中' && cell.zerowidth.contains(&'́') {
                mark_on_lead = true;
            }
            assert!(
                !cell.spacer || cell.zerowidth.is_empty(),
                "spacer must never carry combining marks"
            );
        }
    }
    let snap2 = s.snapshot();
    for cell in snap2.cells.iter() {
        if !cell.spacer && cell.glyph == '中' && cell.zerowidth.contains(&'́') {
            mark_on_lead = true;
        }
        assert!(
            !(cell.spacer && !cell.zerowidth.is_empty()),
            "spacer must never carry combining marks"
        );
    }
    assert!(mark_on_lead, "combining mark must stay on its 中 lead");

    // Widen back: must collapse toward fewer rows without losing wides.
    s.resize(80, 24);
    assert!(s.check_invariants().is_ok());
    let text2 = addressable_text(&s);
    assert_eq!(
        text2.chars().filter(|c| *c == '中').count(),
        40,
        "widen must not lose wides"
    );
}

#[test]
fn scrollback_reflow_neither_duplicates_nor_loses_rows() {
    let mut s = State::new();
    // Clean hard-break lines (CR+LF) so each is its own logical line.
    for i in 0..30 {
        prints(&mut s, &format!("row{i:02}"));
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0D)));
        s.apply(&TerminalAction::PrintControl(ControlChar(0x0A)));
    }
    let sb_before = s.scrollback_len();
    assert!(sb_before > 0);
    // Narrow: short lines stay 1:1, so total rows preserved and each row
    // appears exactly once (no duplication/loss).
    let total_before = sb_before + s.height();
    s.resize(40, 24);
    assert!(s.check_invariants().is_ok());
    assert_eq!(s.width(), 40);
    for line in s.scrollback() {
        assert_eq!(line.cells.len(), 40);
    }
    assert_eq!(
        s.scrollback_len() + s.height(),
        total_before,
        "short-line reflow must preserve total row count"
    );
    // Monotonic ids.
    let mut prev: Option<u64> = None;
    for line in s.scrollback() {
        if let Some(p) = prev {
            assert!(line.id > p);
        }
        prev = Some(line.id);
    }
    // Each logical line findable exactly once across history+grid.
    let text = addressable_text(&s);
    for i in 0..30 {
        let needle = format!("row{i:02}");
        let count = text.matches(&needle).count();
        assert_eq!(
            count, 1,
            "{needle:?} must appear exactly once after reflow, got {count} in {text:?}"
        );
    }
}

#[test]
fn alt_screen_does_not_reflow_application_owns_it() {
    let mut s = State::new();
    prints(&mut s, "PRIMARY");
    // Enter alt screen (1049 clears).
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: true,
    });
    assert!(s.alt_screen_active());
    // Home the alt cursor: entry preserves the primary cursor column (7 after
    // "PRIMARY"), so position explicitly before filling the alt row.
    s.apply(&TerminalAction::CursorPosition {
        row: bitty_vt::Row(1),
        col: bitty_vt::Col(1),
    });
    // Fill alt row 0 with 80 distinct chars.
    let alt_line: String = "A".repeat(80);
    prints(&mut s, &alt_line);
    // Narrow while alt active: alt must truncate (xterm), not rewrap.
    s.resize(40, 24);
    assert_eq!(s.width(), 40);
    assert!(s.alt_screen_active());
    assert!(s.check_invariants().is_ok());
    let snap = s.snapshot();
    // Alt row 0 keeps the first 40, row 1 stays blank (no continuation).
    let row0: String = snap.cells[0..40].iter().map(|c| c.glyph).collect();
    assert_eq!(row0, "A".repeat(40));
    let row1: String = snap.cells[40..80].iter().map(|c| c.glyph).collect();
    assert!(
        row1.trim().is_empty(),
        "alt must not rewrap tail into row 1, got {row1:?}"
    );
    // Leave alt: primary was reflowed (preserved), not truncated.
    s.apply(&TerminalAction::SetMode {
        mode: Mode::AlternateScreenClearAndRestore,
        enabled: false,
    });
    assert!(!s.alt_screen_active());
    assert!(s.check_invariants().is_ok());
    let text = addressable_text(&s);
    assert!(
        text.contains("PRIMARY"),
        "primary content must survive resize while alt was active, got {text:?}"
    );
}
