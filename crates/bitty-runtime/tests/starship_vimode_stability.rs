//! Starship/vim-mode prompt stability (issue #1341).
//!
//! Proves headlessly, without a display server, GPU, fish, or starship:
//!
//! - A fish-style vim-mode repaint (cursor-up + erase-line + full two-line
//!   powerline rewrite with only the mode glyph swapped, the exact redraw
//!   shape captured from live `fish_vi_key_bindings` + starship traffic)
//!   leaves the first prompt line glyph- and style-identical across
//!   insert/normal toggles, and the cursor stays on the command line
//!   (no scroll, no dropped row).
//! - Rapid toggles never corrupt the first line: after every rewrite the
//!   prompt rows match the baseline.
//! - The owned PTY winsize follows the grid on resize (Unix): after
//!   `handle_resize` the kernel winsize equals the presented grid, so the
//!   shell can never lay out a prompt for a size bitty does not render
//!   (the wrap/split class behind vanishing prompt segments).

#![forbid(unsafe_code)]

use bitty_runtime::{Runtime, RuntimeConfig};

/// Insert-mode indicator (starship `character` success symbol shape).
const INSERT_GLYPH: char = '\u{e058}';
/// Normal-mode indicator (starship `vimcmd_symbol` shape).
const NORMAL_GLYPH: char = '\u{e7c5}';
/// Powerline separator (exercises private-use rendering on the prompt row).
const SEPARATOR: char = '\u{e0b4}';

fn make_runtime() -> Runtime {
    Runtime::new(RuntimeConfig {
        theme_resolved: true,
        ..RuntimeConfig::default()
    })
    .expect("headless runtime must build")
}

/// Starship-like first prompt line: SGR truecolor segments joined by
/// powerline separators, ending with SGR reset.
fn prompt_line() -> String {
    format!(
        "\x1b[38;2;0;83;145m\x1b[48;2;0;83;145m TUX \x1b[48;2;70;81;112m\x1b[38;2;0;83;145m{SEPARATOR}\
         \x1b[38;2;255;255;255m ~ \x1b[48;2;95;171;255m\x1b[38;2;70;81;112m{SEPARATOR}\
         \x1b[38;2;70;81;112m branch $?\x1b[0m"
    )
}

/// Second prompt line for one vim mode: mode glyph plus command text.
fn cmdline(mode_glyph: char) -> String {
    format!("\x1b[1;38;2;0;83;145m{mode_glyph}\x1b[0m echo hello world")
}

/// Fish repaint of the two-line prompt as captured live: carriage return,
/// cursor up onto the first line, erase it, rewrite both lines.
fn repaint(mode_glyph: char) -> String {
    format!("\r\x1b[A\x1b[K{}\r\n{}", prompt_line(), cmdline(mode_glyph))
}

/// Glyph + style signature of one grid row for exact comparison.
fn row_signature(rt: &Runtime, row: usize) -> Vec<(char, String)> {
    let snap = rt.snapshot();
    (0..snap.width)
        .map(|c| {
            let cell = &snap.cells[row * snap.width + c];
            (cell.glyph, format!("{:?}", cell.style))
        })
        .collect()
}

fn drive_baseline(rt: &mut Runtime) {
    rt.handle_pty_bytes(b"\x1b[2J\x1b[H");
    let _ = rt.tick();
    rt.handle_pty_bytes(prompt_line().as_bytes());
    rt.handle_pty_bytes(b"\r\n");
    rt.handle_pty_bytes(cmdline(INSERT_GLYPH).as_bytes());
    let _ = rt.tick();
}

#[test]
fn prompt_first_line_stable_across_vim_toggles() {
    let mut rt = make_runtime();
    drive_baseline(&mut rt);

    let snap = rt.snapshot();
    assert_eq!((snap.width, snap.height), (80, 24));
    // Baseline: prompt on row 0, command line on row 1, cursor on row 1.
    assert_eq!(snap.cursor.position.row, 1);
    let first_line = row_signature(&rt, 0);
    // The prompt row is fully drawn: recognizable prompt text is present.
    let first_text: String = first_line.iter().map(|(g, _)| *g).collect();
    assert!(
        first_text.contains('~') && first_text.contains("branch"),
        "prompt row must render prompt text, got {first_text:?}"
    );

    // Alternate normal/insert repaints; only the mode glyph may change.
    for (round, glyph) in [NORMAL_GLYPH, INSERT_GLYPH]
        .iter()
        .cycle()
        .take(8)
        .enumerate()
    {
        rt.handle_pty_bytes(repaint(*glyph).as_bytes());
        let stats = rt.tick();
        assert_eq!(
            row_signature(&rt, 0),
            first_line,
            "first prompt line must survive toggle {round}"
        );
        let snap = rt.snapshot();
        assert_eq!(
            snap.cursor.position.row, 1,
            "cursor must stay on the command line after toggle {round}"
        );
        // The mode glyph on the command line tracks the toggle.
        assert_eq!(snap.cells[snap.width].glyph, *glyph);
        // Damage was noticed: the toggle repainted instead of stalling.
        assert!(
            stats.is_some(),
            "toggle {round} must present damage, not stall the frame"
        );
    }
}

#[test]
fn rapid_toggles_never_scroll_or_drop_rows() {
    let mut rt = make_runtime();
    drive_baseline(&mut rt);
    let first_line = row_signature(&rt, 0);
    let second_line = row_signature(&rt, 1);

    // Rapid Esc/i pairs with a tick per chunk, mirroring the live pump loop.
    for _ in 0..8 {
        rt.handle_pty_bytes(repaint(NORMAL_GLYPH).as_bytes());
        let _ = rt.tick();
        rt.handle_pty_bytes(repaint(INSERT_GLYPH).as_bytes());
        let _ = rt.tick();
    }
    assert_eq!(row_signature(&rt, 0), first_line);
    // Command text (past the one-cell mode glyph) is intact too.
    let after = row_signature(&rt, 1);
    assert_eq!(after[1..], second_line[1..]);
    assert_eq!(rt.snapshot().cursor.position.row, 1);
}

/// The owned PTY winsize follows the grid on resize, so the shell lays out
/// prompts for the size bitty renders (fail-closed against wrap/split
/// prompt corruption from a stale winsize).
#[cfg(unix)]
#[test]
fn owned_pty_winsize_follows_grid_on_resize() {
    use bitty_platform::PhysicalSize;

    let mut rt = make_runtime();
    rt.spawn_shell("/bin/sh")
        .expect("shell must spawn headless");
    let before = rt.pty_size().expect("owned pty reports a size");
    assert_eq!(
        before,
        (80, 24),
        "fresh pty starts at the default grid size"
    );

    rt.handle_resize(PhysicalSize::new(900, 600))
        .expect("resize must succeed");
    let snap = rt.snapshot();
    assert_eq!(
        rt.pty_size(),
        Some((snap.width as u16, snap.height as u16)),
        "kernel winsize must equal the presented grid after resize"
    );
    assert!(snap.width > 0 && snap.height > 0);
}
