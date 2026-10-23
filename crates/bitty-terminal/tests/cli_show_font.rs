//! `bitty +show-font` end-to-end dispatch proofs (CTX-1370, issue #2263).
//!
//! The probe is local/read-only: it loads the merged config and prints stable
//! `font_family:` / `font_size:` / `font_fallback:` lines. These tests drive
//! the built binary and never touch the developer's config (explicit
//! `BITTY_CONFIG` fixture per case).

use std::path::PathBuf;
use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str, config: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bitty-show-font-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("init.lua"), config).unwrap();
        Self { dir }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("init.lua")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run(args: &[&str], config: &PathBuf) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("BITTY_CONFIG", config)
        .env_remove("BITTY_PROFILE")
        .env_remove("BITTY_HEADLESS")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn show_font_reflects_merged_config() {
    let fixture = Fixture::new(
        "config",
        "return { font = { family = 'Maple Mono', size = 14 } }\n",
    );
    let out = run(&["+show-font"], &fixture.config());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "font_family: Maple Mono\nfont_size: 14\n");
}

#[test]
fn show_font_prints_built_in_default_without_explicit_family() {
    let fixture = Fixture::new("default", "return {}\n");
    let out = run(&["+show-font"], &fixture.config());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "font_family: JetBrainsMono Nerd Font\nfont_size: 12\n"
    );
}

#[test]
fn show_font_owner_config_matches_fastfetch_prop_keys() {
    // #2520: the owner's exact font. Detectors read the stable
    // `font_family: ` / `font_size: ` prefixes (fastfetch's
    // `ffParsePropLines`), so the bytes must stay parseable even for an
    // integer-valued fractional-capable size.
    let fixture = Fixture::new(
        "owner-2520",
        "return { theme = 'ayu-mirage', font = { family = 'DejaVu Sans Mono', size = 11 } }\n",
    );
    let out = run(&["+show-font"], &fixture.config());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    assert_eq!(
        text, "font_family: DejaVu Sans Mono\nfont_size: 11\n",
        "the probe must emit exactly the fastfetch-fallback lines"
    );
    let family = text
        .lines()
        .find_map(|line| line.strip_prefix("font_family: "));
    let size = text
        .lines()
        .find_map(|line| line.strip_prefix("font_size: "));
    assert_eq!(family, Some("DejaVu Sans Mono"));
    assert_eq!(size, Some("11"));
}

#[test]
fn show_font_lists_fallback_family() {
    let fixture = Fixture::new(
        "fallback",
        "return { font = { family = 'Maple Mono', size = 13, fallback = { 'Noto Sans Mono' } } }\n",
    );
    let out = run(&["+show-font"], &fixture.config());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "font_family: Maple Mono\nfont_size: 13\nfont_fallback: Noto Sans Mono\n"
    );
}

#[test]
fn show_font_invalid_config_is_a_typed_failure() {
    let fixture = Fixture::new("invalid", "return { font = { family = '', size = 12 } }\n");
    let out = run(&["+show-font"], &fixture.config());
    assert_eq!(out.status.code(), Some(2), "config failures exit 2");
    assert!(stdout(&out).is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("bitty +show-font:"),
        "stderr must name the command"
    );
}

#[test]
fn show_font_rejects_extra_positionals() {
    let fixture = Fixture::new("extra", "return {}\n");
    let out = run(&["+show-font", "unexpected"], &fixture.config());
    assert_eq!(out.status.code(), Some(2));
    assert!(stdout(&out).is_empty());
}

#[test]
fn show_font_help_is_local_and_exit_zero() {
    let fixture = Fixture::new("help", "return {}\n");
    let out = run(&["+show-font", "--help"], &fixture.config());
    assert!(out.status.success());
    assert!(stdout(&out).contains("Usage: bitty +show-font"));
}
