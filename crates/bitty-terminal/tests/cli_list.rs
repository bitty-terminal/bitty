//! `bitty list` end-to-end dispatch proofs (CTX-0172).
//!
//! Canonical: `docs/interfaces/cli.md` (`list` introspection) as
//! refined by `docs/specifications/cli-contract-rfc.md` (mixed class, envelope
//! v1, exit codes, `ls` alias).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`.
//! `list` dispatches before config load and GUI startup, so every case here
//! is headless-safe: no display, no plugin VM, and (for themes/plugins) no
//! instance is touched. `instances` cases use isolated `XDG_RUNTIME_DIR` and
//! `BITTY_SOCKET` fixtures, never the developer's live sockets.

use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn run_bitty(args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(BITTY_BIN);
    cmd.args(args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
}

fn run_bitty_isolated(args: &[&str]) -> Output {
    // Isolate instance discovery from the developer's live sockets:
    // empty temp runtime dir + no advisory socket/instance ids.
    let dir = std::env::temp_dir().join(format!(
        "bitty-list-isolated-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_string_lossy().to_string();
    let out = Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_RUNTIME_DIR", &dir_str)
        .env_remove("BITTY_SOCKET")
        .env_remove("BITTY_INSTANCE_ID")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"));
    let _ = std::fs::remove_dir_all(&dir);
    out
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse_stdout_json(text: &str) -> serde_json_like::Doc {
    serde_json_like::parse(text)
}

/// Minimal JSON field extractor without a new dependency: finds
/// `"key":value` shapes emitted by the envelope (whitespace-free compact
/// form). Panics with the full text on mismatch so failures are actionable.
mod serde_json_like {
    #[derive(Debug)]
    pub struct Doc(String);
    pub fn parse(text: &str) -> Doc {
        let trimmed = text.trim();
        assert!(
            trimmed.starts_with('{') && trimmed.ends_with('}'),
            "stdout must be one JSON object, got {text:?}"
        );
        Doc(trimmed.to_string())
    }
    impl Doc {
        pub fn contains(&self, needle: &str) -> bool {
            self.0.contains(needle)
        }
        pub fn text(&self) -> &str {
            &self.0
        }
    }
}

#[test]
fn list_help_exits_zero() {
    let output = run_bitty(&["list", "--help"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list --help must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty list") && text.contains("themes|plugins|instances"),
        "list --help must describe kinds, got {text:?}"
    );
}

#[test]
fn list_missing_kind_is_usage_error() {
    let output = run_bitty(&["list"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "bare `bitty list` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("missing <kind>"),
        "diagnostic must name the missing kind, got {:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "usage errors must not emit stdout JSON, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_unknown_kind_is_usage_error() {
    let output = run_bitty(&["list", "fonts"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown kind must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("unknown kind"),
        "diagnostic must name the kind error, got {:?}",
        stderr(&output)
    );
}

#[test]
fn list_themes_table_contains_builtin_dark() {
    let output = run_bitty(&["list", "themes"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list themes must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("bitty-dark") && text.contains("#1e1e2e"),
        "themes table must carry the built-in dark preset, got {text:?}"
    );
    assert!(
        text.contains("PROVENANCE") && !text.contains("https://"),
        "themes table must label builtin provenance and print no URLs, got {text:?}"
    );
}

#[test]
fn list_themes_json_envelope_shape() {
    let output = run_bitty(&["list", "themes", "--format", "json"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list themes --format json must exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"v\":1"),
        "envelope version: {:?}",
        doc.text()
    );
    assert!(
        doc.contains("\"command\":\"list\""),
        "command field: {:?}",
        doc.text()
    );
    assert!(doc.contains("\"ok\":true"), "ok field: {:?}", doc.text());
    assert!(
        doc.contains("\"kind\":\"themes\""),
        "kind field: {:?}",
        doc.text()
    );
    assert!(doc.contains("bitty-dark"), "theme name: {:?}", doc.text());
    // Stderr must not corrupt stdout JSON: stdout is exactly one line object.
    assert_eq!(
        stdout(&output).trim().lines().count(),
        1,
        "json must be one value, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_themes_jsonl_is_single_line_envelope() {
    let output = run_bitty(&["list", "themes", "--format", "jsonl"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"v\":1"));
    assert!(doc.contains("\"kind\":\"themes\""));
}

#[test]
fn list_themes_catalog_exposes_30_and_categories() {
    // Pin an empty config root so a developer's local theme files cannot
    // change the built-in catalog count.
    let root = std::env::temp_dir().join(format!(
        "bitty-list-builtin-only-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let root_str = root.to_string_lossy().to_string();
    let output = run_bitty(
        &["list", "themes", "--format", "json"],
        &[("XDG_CONFIG_HOME", &root_str)],
    );
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"count\":30"), "count: {:?}", doc.text());
    // Dark and light presets both ship; provenance and category are exposed.
    assert!(doc.contains("\"category\":\"dark\""));
    assert!(doc.contains("\"category\":\"light\""));
    assert!(doc.contains("tokyo-night"));
    assert!(doc.contains("github-light"));
    assert!(doc.contains("catppuccin-mocha"));
    assert!(doc.contains("https://github.com/folke/tokyonight.nvim"));
    assert!(doc.contains("\"license\":\"MIT\""));
}

#[test]
fn list_plugins_table_contains_bundled() {
    let output = run_bitty(&["list", "plugins"], &[]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "list plugins must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    // #1572 / CTX-0994: shell-integration only; workspace manifest retired.
    assert!(
        text.contains("bitty-terminal.shell-integration"),
        "plugins table must carry bundled ids, got {text:?}"
    );
    assert!(
        !text.contains("bitty-terminal.workspace"),
        "retired workspace must not list, got {text:?}"
    );
}

#[test]
fn list_plugins_json_count_is_one() {
    let output = run_bitty(&["list", "plugins", "--format", "json"], &[]);
    assert_eq!(output.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"kind\":\"plugins\""));
    assert!(doc.contains("\"count\":1"), "count: {:?}", doc.text());
    assert!(doc.contains("bitty-terminal.shell-integration"));
    assert!(!doc.contains("bitty-terminal.workspace"));
    // CTX-0886: Unix philosophy, Core mechanism only. Only shell-integration bundled.
    assert!(!doc.contains("bitty-terminal.palette"));
    assert!(!doc.contains("bitty-terminal.statusline"));
    assert!(!doc.contains("bitty-terminal.git-panel"));
    assert!(!doc.contains("bitty-terminal.file-manager"));
    assert!(!doc.contains("bitty-terminal.ai-panel"));
    assert!(!doc.contains("bitty-terminal.mail-panel"));
    assert!(!doc.contains("bitty-terminal.browser-panel"));
    assert!(!doc.contains("bitty-terminal.project"));
}

#[test]
fn list_instances_empty_dir_is_success_with_count_zero() {
    // Portable parity (CTX-0196): an empty registry is success with count 0
    // on every platform. Unix scans an empty socket dir; Windows scans an
    // empty registry plus live `\\.\pipe\bitty-*` enumeration (no bitty
    // pipes exist in the isolated test, so the merge stays empty).
    let output = run_bitty_isolated(&["list", "instances"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty instances must exit 0, stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).contains("(no instances"),
        "empty table must hint, got {:?}",
        stdout(&output)
    );
    let json = run_bitty_isolated(&["list", "instances", "--format", "json"]);
    assert_eq!(json.status.code(), Some(0));
    let doc = parse_stdout_json(&stdout(&json));
    assert!(doc.contains("\"kind\":\"instances\""));
    assert!(doc.contains("\"count\":0"), "count: {:?}", doc.text());
}

#[test]
fn list_alias_ls_matches_list_for_themes() {
    let canonical = run_bitty(&["list", "themes", "--format", "json"], &[]);
    let alias = run_bitty(&["ls", "themes", "--format", "json"], &[]);
    assert_eq!(canonical.status.code(), Some(0));
    assert_eq!(alias.status.code(), Some(0));
    let canonical_doc = parse_stdout_json(&stdout(&canonical));
    let alias_doc = parse_stdout_json(&stdout(&alias));
    assert!(canonical_doc.contains("\"command\":\"list\""));
    assert!(alias_doc.contains("\"command\":\"ls\""));
    // Same payload apart from the spelling field.
    let strip = |s: &str| {
        s.replace("\"command\":\"list\"", "\"command\":\"X\"")
            .replace("\"command\":\"ls\"", "\"command\":\"X\"")
    };
    assert_eq!(
        strip(&stdout(&canonical)),
        strip(&stdout(&alias)),
        "alias must produce identical payload apart from command spelling"
    );
}

#[test]
fn list_stray_separator_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stray `--` must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn list_unknown_format_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--format", "yaml"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown format must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty(),
        "usage errors must not emit stdout, got {:?}",
        stdout(&output)
    );
}

#[test]
fn list_extra_positional_is_usage_error() {
    let output = run_bitty(&["list", "themes", "extra"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "extra positional must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
fn list_socket_for_non_instances_is_usage_error() {
    let output = run_bitty(&["list", "themes", "--socket", "/tmp/x.sock"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--socket with themes must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
    // The services kind shares the instances-only socket/instance gate.
    let output = run_bitty(&["list", "services", "--socket", "/tmp/x.sock"], &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "--socket with services must be UsageError (exit 2), stderr={:?}",
        stderr(&output)
    );
}

#[test]
// Portable: Unix probes the missing socket path, Windows probes the missing
// pipe/registry path; both report Unavailable (exit 6) with an ok:false
// envelope.
fn list_explicit_missing_socket_is_runtime_unavailable() {
    let output = run_bitty_isolated(&[
        "list",
        "instances",
        "--socket",
        "/tmp/bitty-list-definitely-missing-9f8e7d.sock",
        "--format",
        "json",
    ]);
    assert_eq!(
        output.status.code(),
        Some(6),
        "missing explicit socket must be exit 6, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"ok\":false"), "ok field: {:?}", doc.text());
    assert!(
        doc.contains("\"class\":\"Unavailable\""),
        "class: {:?}",
        doc.text()
    );
}

#[test]
fn list_themes_table_omits_swatches_when_piped_but_keeps_hex() {
    // CTX-1065 unified gate: captured (piped) stdout is not a TTY, so even
    // with color-capable TERM and NO_COLOR removed the table carries no
    // swatches. Forced-color rendering stays covered by unit tests.
    // Pin per child process (never the parent): TERM advertises color while
    // NO_COLOR is removed, so the assertion is deterministic regardless of
    // the ambient CI environment (same pattern as cli_help.rs).
    let output = Command::new(BITTY_BIN)
        .args(["list", "themes"])
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .output()
        .expect("spawn bitty list themes with color");
    assert_eq!(
        output.status.code(),
        Some(0),
        "list themes must exit 0, stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        !text.contains('\u{1b}'),
        "piped themes table must carry no swatches under the unified gate, got {text:?}"
    );
    // Hex stays greppable alongside the swatches.
    assert!(text.contains("bitty-dark"));
    assert!(text.contains("#1e1e2e"));
}

#[test]
fn list_themes_no_color_flag_strips_swatches_but_keeps_hex() {
    let output = Command::new(BITTY_BIN)
        .args(["list", "themes", "--no-color"])
        .env("TERM", "xterm-256color")
        .env_remove("NO_COLOR")
        .output()
        .expect("spawn bitty list themes --no-color");
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        !text.contains('\u{1b}'),
        "--no-color must strip swatches, got {text:?}"
    );
    assert!(text.contains("bitty-dark"));
    assert!(text.contains("#1e1e2e"));
}

#[test]
fn list_themes_no_color_env_strips_swatches() {
    let output = Command::new(BITTY_BIN)
        .args(["list", "themes"])
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn bitty list themes with NO_COLOR=1");
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        !text.contains('\u{1b}'),
        "NO_COLOR=1 must strip swatches, got {text:?}"
    );
    assert!(text.contains("#1e1e2e"));
}

#[test]
fn list_themes_term_dumb_strips_swatches() {
    let output = Command::new(BITTY_BIN)
        .args(["list", "themes"])
        .env("TERM", "dumb")
        .env_remove("NO_COLOR")
        .output()
        .expect("spawn bitty list themes with TERM=dumb");
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    assert!(
        !text.contains('\u{1b}'),
        "TERM=dumb must strip swatches, got {text:?}"
    );
    assert!(text.contains("#1e1e2e"));
}

// ---------------------------------------------------------------------------
// Local theme files (CTX-2464, OQ-047)
// ---------------------------------------------------------------------------

fn local_theme_fixture(background: &str) -> String {
    let ansi = [
        "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
        "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#cdd6f4",
    ]
    .iter()
    .map(|value| format!("\"{value}\""))
    .collect::<Vec<_>>()
    .join(", ");
    format!(
        "name = \"sunset\"\ncategory = \"dark\"\n\
         source = \"https://example.invalid/sunset\"\nlicense = \"MIT\"\n\
         background = \"{background}\"\nforeground = \"#cdd6f4\"\n\
         cursor = \"#f5e0dc\"\nselection = \"#313244\"\nansi = [{ansi}]\n"
    )
}

fn themed_xdg_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "bitty-list-local-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let themes = root.join("bitty").join("themes");
    std::fs::create_dir_all(&themes).expect("themes dir");
    std::fs::write(themes.join("sunset.toml"), local_theme_fixture("#1e1e2e"))
        .expect("write local theme");
    root
}

#[test]
fn list_themes_includes_local_files_without_network_provenance_in_table() {
    let root = themed_xdg_root("table");
    let root_str = root.to_string_lossy().to_string();
    let output = run_bitty(&["list", "themes"], &[("XDG_CONFIG_HOME", &root_str)]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={:?}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("PROVENANCE"), "table header: {text:?}");
    assert!(
        text.lines().any(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            columns.len() >= 4
                && columns[0] == "local:sunset"
                && columns[1] == "dark"
                && columns[3] == "local"
        }),
        "local row must carry origin local, got {text:?}"
    );
    assert!(
        text.lines().any(|line| {
            let columns: Vec<_> = line.split_whitespace().collect();
            columns.len() >= 4 && columns[0] == "bitty-dark" && columns[3] == "builtin"
        }),
        "builtin row must carry origin builtin, got {text:?}"
    );
    assert!(
        !text.contains("https://"),
        "the human table must not look like it fetches upstream URLs: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_themes_json_marks_local_source_and_keeps_url_as_provenance() {
    let root = themed_xdg_root("json");
    let root_str = root.to_string_lossy().to_string();
    let output = run_bitty(
        &["list", "themes", "--format", "json"],
        &[("XDG_CONFIG_HOME", &root_str)],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"name\":\"local:sunset\"") && doc.contains("\"source\":\"local\""),
        "{:?}",
        doc.text()
    );
    assert!(
        doc.contains("\"provenance\":\"https://example.invalid/sunset\""),
        "{:?}",
        doc.text()
    );
    assert!(
        doc.contains("\"source\":\"builtin\""),
        "builtin rows keep a stable origin: {:?}",
        doc.text()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn safe_list_themes_reads_no_local_files() {
    let root = themed_xdg_root("safe");
    let root_str = root.to_string_lossy().to_string();
    let output = run_bitty(
        &["--safe", "list", "themes", "--format", "json"],
        &[("XDG_CONFIG_HOME", &root_str)],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"count\":30") && !doc.contains("local:sunset"),
        "--safe must keep the catalog built-in-only: {:?}",
        doc.text()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_themes_reports_a_rejected_local_file_and_stays_successful() {
    let root = themed_xdg_root("bad");
    std::fs::write(
        root.join("bitty").join("themes").join("broken.toml"),
        "name = \"broken\"\nbackground = \"#123456\"\n",
    )
    .expect("write broken theme");
    let root_str = root.to_string_lossy().to_string();
    let output = run_bitty(&["list", "themes"], &[("XDG_CONFIG_HOME", &root_str)]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr={:?}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("broken.toml"), "diagnostic: {err:?}");
    assert!(
        err.contains("missing required field"),
        "diagnostic: {err:?}"
    );
    assert!(stdout(&output).contains("local:sunset"));
    let _ = std::fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// `bitty list services` (dynapi observability, bitty#2002)
// ---------------------------------------------------------------------------

/// Isolated fixture roots: plugin dir plus blank XDG homes so the listing
/// never touches the developer's store, config, or sockets. No plugin VM
/// ever loads: discovery parses manifests only.
struct ServicesFixture {
    root: std::path::PathBuf,
}

impl ServicesFixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "bitty-list-services-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["plugins", "data", "config"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self { root }
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![
            (
                "BITTY_PLUGIN_DIR".to_string(),
                self.root.join("plugins").to_string_lossy().to_string(),
            ),
            (
                "XDG_DATA_HOME".to_string(),
                self.root.join("data").to_string_lossy().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                self.root.join("config").to_string_lossy().to_string(),
            ),
        ]
    }

    fn write_plugin(&self, id: &str, manifest_extra: &str, capabilities: &str) {
        self.write_plugin_as(id, id, manifest_extra, capabilities);
    }

    /// Variant with an explicit on-disk directory name. Hostile-id fixtures
    /// need this: Win32 rejects control characters in a path component
    /// (`ERROR_INVALID_NAME`) before the discovery path under test runs, so the
    /// manifest content — not the directory name — must carry the hostile value
    /// on every OS.
    fn write_plugin_as(&self, dir: &str, id: &str, manifest_extra: &str, capabilities: &str) {
        let plugin = self.root.join("plugins").join(dir);
        std::fs::create_dir_all(plugin.join("lua")).unwrap();
        std::fs::write(
            plugin.join("bitty-plugin.toml"),
            format!(
                "[plugin]\nid = \"{id}\"\nname = \"Test\"\nversion = \"0.1.0\"\ndescription = \"test\"\n\n[compat]\nplugin-api = \"^1.0\"\n\n{manifest_extra}\n[capabilities]\n{capabilities}\n[lazy]\ncommands = [\"{id}:ping\"]\nevents = []\n"
            ),
        )
        .unwrap();
        std::fs::write(
            plugin.join("lua/init.lua"),
            "error(\"sentinel: list services must not execute plugin VM\")\nreturn {}\n",
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let env = self.env();
        let refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let output = run_bitty(args, &refs);
        assert_eq!(
            output.status.code(),
            Some(0),
            "list services must succeed without executing plugin VM, args={args:?}, stderr={:?}",
            stderr(&output)
        );
        output
    }
}

impl Drop for ServicesFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const CALC_SERVICES: &str =
    "[services.provided]\n\"calc.add\" = { version = \"1.0.0\", heads = [\"runtime.inspect\"] }\n";

#[test]
fn list_services_empty_without_fixtures_is_success_with_count_zero() {
    // No bundled provision exists today, so a blank discovery is an empty
    // success — exit 0, count 0, frozen keys present.
    let fixture = ServicesFixture::new("empty");
    let output = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "empty services must exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"kind\":\"services\""));
    assert!(doc.contains("\"count\":0"), "count: {:?}", doc.text());
    assert!(doc.contains("\"services\":[]"));
    let table = fixture.run(&["list", "services"]);
    assert_eq!(table.status.code(), Some(0));
    assert!(
        stdout(&table).contains("(no services)"),
        "empty table must hint, got {:?}",
        stdout(&table)
    );
}

#[test]
fn list_services_registered_rows_carry_frozen_keys() {
    // Register (fixture present) -> listed with every frozen key; the
    // sentinel init.lua beside it never executes (every run asserts exit 0).
    let fixture = ServicesFixture::new("registered");
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    let output = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "services must exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(doc.contains("\"kind\":\"services\""));
    assert!(doc.contains("\"count\":1"), "count: {:?}", doc.text());
    for needle in [
        "\"interface\":\"calc.add\"",
        "\"owner\":\"xuepoo\"",
        "\"version\":\"1.0.0\"",
        "\"provider\":\"xuepoo.calc\"",
        "\"backing_head\":\"runtime.inspect\"",
        "\"grant_source\":\"manifest\"",
        "\"status\":\"active\"",
    ] {
        assert!(doc.contains(needle), "row {needle}: {:?}", doc.text());
    }
    assert!(!stdout(&output).contains("null"));
    let table = fixture.run(&["list", "services"]);
    assert_eq!(table.status.code(), Some(0));
    let text = stdout(&table);
    assert!(
        text.contains("INTERFACE OWNER VERSION PROVIDER BACKING_HEAD GRANT_SOURCE STATUS"),
        "header: {text:?}"
    );
    assert!(text.contains("calc.add"), "row: {text:?}");
    assert!(text.contains("xuepoo.calc"), "row: {text:?}");
}

#[test]
fn list_services_ungranted_is_absent_not_denied() {
    // A provision naming an ungranted head is absent (not "denied"):
    // indistinguishable from undeclared, the same posture as services.get.
    let fixture = ServicesFixture::new("absent");
    fixture.write_plugin(
        "xuepoo.spoof",
        "[services.provided]\n\"calc.spoof\" = { version = \"1.0.0\", heads = [\"workspace.read\"] }\n",
        "",
    );
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    let output = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "listing stays exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"count\":1"),
        "only granted rows: {:?}",
        doc.text()
    );
    assert!(
        !doc.contains("calc.spoof"),
        "ungranted absent: {:?}",
        doc.text()
    );
    assert!(
        !doc.contains("denied"),
        "no denial posture: {:?}",
        doc.text()
    );
}

#[test]
fn list_services_invalid_manifest_fails_root_closed_without_execution() {
    // An invalid manifest fails its discovery root closed (nothing from
    // that root lists — the same all-or-nothing the runtime load path
    // enforces), stays exit 0, and never executes any payload.
    let fixture = ServicesFixture::new("broken");
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    let broken = fixture.root.join("plugins").join("xuepoo.broken");
    std::fs::create_dir_all(broken.join("lua")).unwrap();
    std::fs::write(broken.join("bitty-plugin.toml"), "not = [valid toml\n").unwrap();
    std::fs::write(broken.join("lua/init.lua"), "return {}\n").unwrap();
    let output = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "broken manifest stays exit 0, stderr={:?}",
        stderr(&output)
    );
    let doc = parse_stdout_json(&stdout(&output));
    assert!(
        doc.contains("\"count\":0"),
        "root fails closed: {:?}",
        doc.text()
    );
}

#[test]
fn list_services_unregister_is_gone_and_safe_skips_third_party() {
    // Unregister (fixture removed) -> gone; `--safe` skips third-party
    // sources exactly like startup while staying exit 0.
    let fixture = ServicesFixture::new("gone");
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    let before = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(before.status.code(), Some(0));
    assert!(parse_stdout_json(&stdout(&before)).contains("\"count\":1"));
    std::fs::remove_dir_all(fixture.root.join("plugins").join("xuepoo.calc")).unwrap();
    let after = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(after.status.code(), Some(0));
    assert!(
        parse_stdout_json(&stdout(&after)).contains("\"count\":0"),
        "unregistered gone: {:?}",
        stdout(&after)
    );
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    let safe = fixture.run(&["--safe", "list", "services", "--format", "json"]);
    assert_eq!(safe.status.code(), Some(0));
    assert!(
        parse_stdout_json(&stdout(&safe)).contains("\"count\":0"),
        "safe skips third-party: {:?}",
        stdout(&safe)
    );
}

#[test]
fn list_services_control_char_id_is_rejected_safely() {
    // A hostile provider id (control characters) cannot register and
    // cannot inject display text: discovery rejects it fail-closed and
    // the listing stays exit 0 without naming it.
    let fixture = ServicesFixture::new("hostile");
    // The hostile value lives in the manifest only: a control character in a
    // directory name is itself invalid on Win32, which would fail the fixture
    // before the discovery path under test is exercised.
    fixture.write_plugin_as(
        "xuepoo.evilcalc",
        "xuepoo.evil\x07calc",
        CALC_SERVICES,
        "runtime.inspect = true\n",
    );
    let output = fixture.run(&["list", "services", "--format", "json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "hostile id must not break the listing, stderr={:?}",
        stderr(&output)
    );
    assert!(
        parse_stdout_json(&stdout(&output)).contains("\"count\":0"),
        "hostile unregistered: {:?}",
        stdout(&output)
    );
    assert!(
        !stdout(&output).contains('\u{7}'),
        "no raw control chars reach stdout: {:?}",
        stdout(&output)
    );
}

#[test]
fn list_services_singular_and_ls_alias_match() {
    let fixture = ServicesFixture::new("alias");
    fixture.write_plugin("xuepoo.calc", CALC_SERVICES, "runtime.inspect = true\n");
    for args in [&["list", "service"] as &[&str], &["ls", "services"]] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(
            stdout(&output).contains("calc.add"),
            "{args:?}: {:?}",
            stdout(&output)
        );
    }
}
