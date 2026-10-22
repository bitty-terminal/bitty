//! Binary-level theme readability diagnostics (CTX-1418, issue #2370).
//!
//! `bitty config check` must report the same policy the `bitty-config`
//! catalog test enforces: a shipped preset passes, an unreadable inline
//! `appearance.colors` palette names the failing ANSI role, and an advisory
//! neutral surface never turns the result into a failure.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// A complete inline palette based on GitHub Light, with ANSI 12 replaced by
/// a near-white that cannot clear 4.5:1 on the white background.
const LOW_CONTRAST_PALETTE: &str = r##"return {
  appearance = {
    theme = "github-light",
    colors = {
      background = "#FFFFFF",
      foreground = "#1F2328",
      cursor = "#0969DA",
      selection = "#B6D7FF",
      ansi = {
        "#24292F", "#CF222E", "#116329", "#4D2D00",
        "#0969DA", "#8250DF", "#1B7C83", "#6E7781",
        "#57606A", "#A40E26", "#1A7F37", "#633C01",
        "#EEEEEE", "#8861CE", "#2B8096", "#8C959F",
      },
    },
  },
}
"##;

fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-ctx1418-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn run_bitty(root: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", root)
        .env("HOME", root)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

/// One `config check` row value for `key` (e.g. `theme.contrast.result`).
fn row(text: &str, key: &str) -> String {
    let prefix = format!("{key} = ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing row {key:?} in output:\n{text}"))
        .trim()
        .to_string()
}

#[test]
fn shipped_preset_reports_contrast_pass() {
    let root = scratch_root("preset");
    let output = run_bitty(&root, &["--theme", "github-light", "config", "check"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(row(&text, "theme.contrast.result"), "pass (policy)");
    assert!(text.contains("theme.contrast = "));
    // Solarized is the only family with a measured exception; the neutral
    // surface rows are advisory and must not fabricate a failure.
    assert!(!text.contains("theme.contrast.violation."));
}

#[test]
fn unreadable_custom_palette_names_the_failing_ansi_role() {
    let root = scratch_root("custom");
    let config = root.join("init.lua");
    std::fs::write(&config, LOW_CONTRAST_PALETTE).expect("write init.lua");
    let config_str = config.to_str().expect("utf8 config path");

    let output = run_bitty(&root, &["config", "check", "--config", config_str]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("theme.contrast.violation.ansi12.text"),
        "custom palette violation must name ansi12.text:\n{text}"
    );
    assert_ne!(row(&text, "theme.contrast.result"), "pass (policy)");
    assert!(row(&text, "theme.contrast.result").contains("1 contrast violation(s)"));
}

#[test]
fn neutral_surface_below_the_floor_is_advisory_not_a_violation() {
    // Tokyo Night's ANSI 0 sits 1.05:1 from its background by upstream
    // design. It is reported as advisory, and the preset still passes.
    let root = scratch_root("surface");
    let output = run_bitty(&root, &["--theme", "tokyo-night", "config", "check"]);
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(row(&text, "theme.contrast.result"), "pass (policy)");
    assert!(text.contains("theme.contrast.advisory.ansi0.surface"));
    assert!(!text.contains("theme.contrast.violation."));
}

fn write_local_theme(root: &Path, name: &str, foreground: &str) {
    let themes = root.join("bitty").join("themes");
    std::fs::create_dir_all(&themes).expect("themes dir");
    let ansi = [
        "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
        "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
    ]
    .iter()
    .map(|value| format!("\"{value}\""))
    .collect::<Vec<_>>()
    .join(", ");
    std::fs::write(
        themes.join(format!("{name}.toml")),
        format!(
            "name = \"{name}\"\ncategory = \"dark\"\nbackground = \"#1e1e2e\"\n\
             foreground = \"{foreground}\"\ncursor = \"#f5e0dc\"\n\
             selection = \"#313244\"\nansi = [{ansi}]\n"
        ),
    )
    .expect("write local theme");
}

#[test]
fn readable_local_theme_passes_the_same_policy() {
    let root = scratch_root("local-pass");
    write_local_theme(&root, "sunset", "#cdd6f4");
    let init = root.join("bitty").join("init.lua");
    std::fs::write(
        &init,
        "return { appearance = { theme = \"local:sunset\" } }\n",
    )
    .expect("write init.lua");
    let init_str = init.to_str().expect("utf8 config path");

    let output = run_bitty(&root, &["config", "check", "--config", init_str]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("appearance.theme = \"local:sunset\""),
        "local selection must be reported:\n{text}"
    );
    assert_eq!(row(&text, "theme.contrast.result"), "pass (policy)");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn unreadable_local_theme_is_rejected_at_load_with_a_reason() {
    let root = scratch_root("local-fail");
    write_local_theme(&root, "dim", "#202030");
    let init = root.join("bitty").join("init.lua");
    std::fs::write(&init, "return { appearance = { theme = \"local:dim\" } }\n")
        .expect("write init.lua");
    let init_str = init.to_str().expect("utf8 config path");

    let output = run_bitty(&root, &["config", "check", "--config", init_str]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("readability") && err.contains("4.5:1"),
        "rejected local theme must name the readability floor:\n{err}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn light_local_theme_with_its_own_borders_passes_without_unknown_theme_log() {
    let root = scratch_root("local-light");
    let themes = root.join("bitty").join("themes");
    std::fs::create_dir_all(&themes).expect("themes dir");
    std::fs::write(
        themes.join("paper.toml"),
        "name = \"paper\"\ncategory = \"light\"\n\
         background = \"#FFFFFF\"\nforeground = \"#1F2328\"\n\
         cursor = \"#0969DA\"\nselection = \"#B6D7FF\"\n\
         ansi = [\"#24292F\", \"#CF222E\", \"#116329\", \"#4D2D00\", \
         \"#0969DA\", \"#8250DF\", \"#1B7C83\", \"#6E7781\", \
         \"#57606A\", \"#A40E26\", \"#1A7F37\", \"#633C01\", \
         \"#1C76D9\", \"#8861CE\", \"#2B8096\", \"#8C959F\"]\n\
         border_focused = \"#0969DA\"\nborder_idle = \"#1F2328\"\n",
    )
    .expect("write light local theme");
    let init = root.join("bitty").join("init.lua");
    std::fs::write(
        &init,
        "return { appearance = { theme = \"local:paper\" } }\n",
    )
    .expect("write init.lua");
    let init_str = init.to_str().expect("utf8 config path");

    let output = run_bitty(&root, &["config", "check", "--config", init_str]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(row(&text, "theme.contrast.result"), "pass (policy)");
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        !err.contains("unknown theme"),
        "a valid local selection must not log an unknown-theme fallback: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
