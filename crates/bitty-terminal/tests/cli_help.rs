//! CTX-1060 (issue #1808): compact default `bitty --help`.
//!
//! The default help is a short overview (usage + common flags + one line per
//! subcommand, ~40 lines) with bold group headers. Full flag detail is
//! reachable via `bitty --help -v` / `bitty --help --verbose`, and every
//! subcommand keeps its own `bitty <command> --help`. `--no-color`,
//! `NO_COLOR=1`, and `TERM=dumb` all strip the ANSI emphasis, mirroring the
//! table color conventions in `list`/`plugin`.
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`;
//! every case dispatches before config load, runtime creation, and GUI
//! startup, so all are headless-safe.

use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Runs `bitty` with `args`, capturing output.
///
/// The color cases pin the environment per child process (never the parent):
/// `TERM=xterm-256color` advertises color support while `NO_COLOR` is
/// removed, so the default-color assertion is deterministic regardless of
/// the ambient CI environment.
fn run_bitty(args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

#[test]
fn default_help_fits_two_screen_budget() {
    let output = run_bitty(&["--help"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "--help must exit 0, stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = stdout(&output);
    assert!(help.contains("Usage:"), "default help must show usage");
    assert!(
        help.contains("Subcommands"),
        "default help must list subcommands"
    );
    let lines = line_count(&help);
    assert!(
        lines <= 60,
        "default help must fit ~2 screens (<= 60 lines), got {lines}:\n{help}"
    );
}

#[test]
fn default_help_lists_every_subcommand_on_one_line() {
    let help = stdout(&run_bitty(&["--help"]));
    for sub in [
        "run",
        "ctl",
        "config",
        "init",
        "doctor",
        "list",
        "inspect",
        "dev",
        "plugin",
        "component",
        "cmd",
        "x",
        "completion",
        "shell-init",
        "version",
    ] {
        assert!(
            help.lines().any(|line| line.trim_start().starts_with(sub)),
            "default help must carry a one-liner for subcommand {sub:?}:\n{help}"
        );
    }
}

#[test]
fn default_help_is_colorized_but_no_color_strips_ansi() {
    let colored = stdout(&run_bitty(&["--help"]));
    assert!(
        colored.contains("\u{1b}["),
        "default help should carry ANSI group headers when color is supported:\n{colored}"
    );

    // Explicit flag wins.
    let plain = stdout(&run_bitty(&["--help", "--no-color"]));
    assert!(
        !plain.contains("\u{1b}"),
        "--no-color must strip ANSI, got {plain:?}"
    );

    // `NO_COLOR` env wins (codebase convention, also honored by tables).
    let no_color_env = Command::new(BITTY_BIN)
        .args(["--help"])
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn bitty --help with NO_COLOR=1");
    let no_color_help = stdout(&no_color_env);
    assert!(
        !no_color_help.contains("\u{1b}"),
        "NO_COLOR=1 must strip ANSI, got {no_color_help:?}"
    );

    // `TERM=dumb` wins (same convention as list/plugin tables).
    let dumb = Command::new(BITTY_BIN)
        .args(["--help"])
        .env("TERM", "dumb")
        .env_remove("NO_COLOR")
        .output()
        .expect("spawn bitty --help with TERM=dumb");
    let dumb_help = stdout(&dumb);
    assert!(
        !dumb_help.contains("\u{1b}"),
        "TERM=dumb must strip ANSI, got {dumb_help:?}"
    );
}

#[test]
fn verbose_help_restores_full_detail() {
    let short_lines = line_count(&stdout(&run_bitty(&["--help"])));
    for args in [&["--help", "-v"][..], &["--help", "--verbose"][..]] {
        let output = run_bitty(args);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?} must exit 0, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        let full = stdout(&output);
        assert!(
            full.contains("--split-ratio"),
            "{args:?} must restore full flag detail (want --split-ratio):\n{full}"
        );
        assert!(
            full.contains("--font-family"),
            "{args:?} must restore full flag detail (want --font-family):\n{full}"
        );
        let full_lines = line_count(&full);
        assert!(
            full_lines >= 150,
            "{args:?} must restore the full text (>= 150 lines), got {full_lines}"
        );
        assert!(
            full_lines > short_lines,
            "{args:?} ({full_lines} lines) must be longer than the short overview ({short_lines} lines)"
        );
    }
}

#[test]
fn every_subcommand_still_reachable_via_per_command_help() {
    // (subcommand, marker that proves its dedicated help rendered;
    // `config`/`init`/`doctor` fall back to the top-level help, which must
    // still exit 0 with usage on stdout.)
    let cases: &[(&str, &str)] = &[
        ("run", "bitty run"),
        ("ctl", "bitty ctl"),
        ("config", "Usage:"),
        ("init", "Usage:"),
        ("doctor", "Usage:"),
        ("list", "bitty list"),
        ("inspect", "bitty inspect"),
        ("dev", "bitty dev"),
        ("plugin", "bitty plugin"),
        ("component", "bitty component"),
        ("cmd", "bitty cmd"),
        ("x", "bitty x"),
        ("completion", "bitty completion"),
        ("shell-init", "bitty shell-init"),
        ("version", "bitty version"),
    ];
    for (sub, marker) in cases {
        let output = run_bitty(&[sub, "--help"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "bitty {sub} --help must exit 0, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        let help = stdout(&output);
        assert!(
            !help.trim().is_empty(),
            "bitty {sub} --help must print help to stdout"
        );
        assert!(
            help.contains(marker),
            "bitty {sub} --help must contain {marker:?}, got:\n{help}"
        );
    }
}
