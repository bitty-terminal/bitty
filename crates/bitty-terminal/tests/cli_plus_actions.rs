//! `bitty +<action>` helper aliases end-to-end proofs (CTX-1467, issue #2464).
//!
//! These tests drive the built `bitty` binary via `CARGO_BIN_EXE_bitty`. Every
//! case isolates XDG/HOME into a scratch directory so the developer's config,
//! themes, and editor never participate. All actions are local-class: no
//! display, no instance, no plugin VM, and no network.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Isolated environment for one test case.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "bitty-plus-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        for child in ["home", "cfg", "data", "state", "run"] {
            std::fs::create_dir_all(root.join(child)).expect("scratch dir");
        }
        Self { root }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("cfg/bitty")
    }

    fn config_path(&self) -> PathBuf {
        self.config_dir().join("init.lua")
    }

    fn write_config(&self, content: &str) {
        std::fs::create_dir_all(self.config_dir()).expect("config dir");
        std::fs::write(self.config_path(), content).expect("write config");
    }

    fn write_profile(&self, name: &str, content: &str) -> PathBuf {
        let dir = self.config_dir().join("profiles");
        std::fs::create_dir_all(&dir).expect("profile dir");
        let path = dir.join(format!("{name}.lua"));
        std::fs::write(&path, content).expect("write profile");
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(BITTY_BIN);
        cmd.args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("cfg"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env_remove("BITTY_CONFIG")
            .env_remove("BITTY_PROFILE")
            .env_remove("BITTY_SOCKET")
            .env_remove("BITTY_INSTANCE_ID")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("NO_COLOR");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args)
            .output()
            .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?} {args:?}: {err}"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Payload suffix shared by the alias and its canonical spelling.
fn result_payload(text: &str) -> &str {
    text.split_once("\"result\":")
        .unwrap_or_else(|| panic!("missing result envelope: {text:?}"))
        .1
}

/// Extracts one flat key-attribution row from the `config show` envelope.
///
/// Row values are scalars or string arrays (no nested objects), so the first
/// closing brace after the key marker ends the row.
fn json_row<'a>(text: &'a str, key: &str) -> &'a str {
    let marker = format!("\"key\":\"{key}\"");
    let position = text
        .find(&marker)
        .unwrap_or_else(|| panic!("missing row {key:?}: {text}"));
    let start = text[..position]
        .rfind('{')
        .unwrap_or_else(|| panic!("row {key:?} has no object start: {text}"));
    let end = text[position..]
        .find('}')
        .map(|offset| position + offset)
        .unwrap_or_else(|| panic!("row {key:?} has no object end: {text}"));
    &text[start..=end]
}

/// Decodes one JSON string field so Windows path separators compare equal to
/// the source path (the envelope escapes `\` as `\\`).
fn json_string_field(object: &str, field: &str) -> String {
    let marker = format!("\"{field}\":\"");
    let start = object
        .find(&marker)
        .unwrap_or_else(|| panic!("missing field {field:?}: {object}"))
        + marker.len();
    let mut out = String::new();
    let mut chars = object[start..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return out,
            '\\' => match chars.next().expect("JSON escape") {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16)
                        .unwrap_or_else(|_| panic!("bad \\u escape {hex:?}"));
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => panic!("unsupported JSON escape \\{other}"),
            },
            other => out.push(other),
        }
    }
    panic!("unterminated JSON string for {field:?}: {object}");
}

fn assert_success(output: &Output, context: &str) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{context} failed: stderr={}",
        stderr(output)
    );
}

#[test]
fn list_aliases_emit_the_same_payload_as_canonical_commands() {
    let fixture = Fixture::new("aliases");
    let cases: &[(&str, &[&str])] = &[
        ("+list-themes", &["list", "themes"]),
        ("+list-colors", &["list", "colors"]),
        ("+list-fonts", &["list", "fonts"]),
        ("+list-keybinds", &["list", "keymaps"]),
        ("+list-actions", &["list", "actions"]),
        ("+list-plugins", &["list", "plugins"]),
        ("+list-extensions", &["extension", "list"]),
    ];
    for (alias, canonical) in cases {
        let alias_out = fixture.run(&[alias, "--format", "json"]);
        assert_success(&alias_out, alias);
        let mut canonical_args: Vec<&str> = canonical.to_vec();
        canonical_args.extend(["--format", "json"]);
        let canonical_out = fixture.run(&canonical_args);
        assert_success(&canonical_out, &canonical_args.join(" "));
        let alias_text = stdout(&alias_out);
        let canonical_text = stdout(&canonical_out);
        // `+list-extensions` reuses the frozen extension envelope whose
        // command id stays the canonical `component`; list aliases name the
        // invoked spelling like `ls` does.
        let expected_command = if *alias == "+list-extensions" {
            "component"
        } else {
            alias
        };
        assert!(
            alias_text.contains(&format!("\"command\":\"{expected_command}\"")),
            "{alias} must carry the stable command id: {alias_text}"
        );
        assert_eq!(
            result_payload(&alias_text),
            result_payload(&canonical_text),
            "{alias} must reuse the canonical result payload (no parallel logic)"
        );
    }
}

#[test]
fn config_aliases_emit_the_same_payload_as_canonical_commands() {
    let fixture = Fixture::new("config-aliases");
    fixture.write_config("return { font = { family = \"JetBrains Mono\", size = 13 } }\n");

    let show_alias = fixture.run(&["+show-config", "--format", "json"]);
    assert_success(&show_alias, "+show-config");
    let show_canonical = fixture.run(&["config", "show", "--format", "json"]);
    assert_success(&show_canonical, "config show");
    assert!(stdout(&show_alias).contains("\"command\":\"+show-config\""));
    assert_eq!(
        result_payload(&stdout(&show_alias)),
        result_payload(&stdout(&show_canonical))
    );

    let validate_alias = fixture.run(&["+validate-config", "--format", "json"]);
    assert_success(&validate_alias, "+validate-config");
    let validate_canonical = fixture.run(&["config", "validate", "--format", "json"]);
    assert_success(&validate_canonical, "config validate");
    assert_eq!(
        result_payload(&stdout(&validate_alias)),
        result_payload(&stdout(&validate_canonical))
    );

    // Text `+show-config` is byte-identical to `config check` (same rows).
    let show_text = fixture.run(&["+show-config"]);
    assert_success(&show_text, "+show-config (text)");
    let check_text = fixture.run(&["config", "check"]);
    assert_success(&check_text, "config check");
    assert_eq!(stdout(&show_text), stdout(&check_text));
}

#[test]
fn json_schemas_stay_stable_for_every_static_action() {
    let fixture = Fixture::new("schema");
    let cases: &[(&str, &str)] = &[
        ("+list-themes", "\"kind\":\"themes\""),
        ("+list-colors", "\"kind\":\"colors\",\"theme\":"),
        ("+list-fonts", "\"kind\":\"fonts\",\"supported\":true"),
        ("+list-keybinds", "\"kind\":\"keymaps\""),
        ("+list-actions", "\"kind\":\"actions\""),
        ("+list-plugins", "\"kind\":\"plugins\""),
        ("+list-extensions", "\"result\":{\"verb\":\"list\""),
        ("+show-config", "\"kind\":\"config\",\"valid\":true"),
        ("+validate-config", "\"kind\":\"config\",\"valid\":true"),
    ];
    for (alias, marker) in cases {
        let out = fixture.run(&[alias, "--format", "json"]);
        assert_success(&out, alias);
        let text = stdout(&out);
        let expected_command = if *alias == "+list-extensions" {
            "component"
        } else {
            alias
        };
        assert!(
            text.starts_with(&format!(
                "{{\"v\":1,\"command\":\"{expected_command}\",\"ok\":true,"
            )),
            "{alias} envelope must start with v/command/ok: {text}"
        );
        assert!(
            text.contains(marker),
            "{alias} envelope must carry {marker:?}: {text}"
        );
        assert!(
            text.trim_end().ends_with('}'),
            "{alias} must emit one JSON object: {text}"
        );
        assert!(
            !stderr(&out).contains("panicked"),
            "{alias} must never panic: {}",
            stderr(&out)
        );
    }

    // Colors: exactly the 16 ANSI entries plus UI roles, all with attribution.
    let colors = stdout(&fixture.run(&["+list-colors", "--format", "json"]));
    assert!(colors.contains("\"count\":23"), "{colors}");
    for index in 0..16 {
        assert!(
            colors.contains(&format!("\"ansi{index}.")),
            "missing ANSI slot {index}: {colors}"
        );
    }
    for token in [
        "foreground",
        "cursor",
        "selection.text",
        "selection.surface",
        "chrome.help-ring",
        "chrome.banner-text",
    ] {
        assert!(
            colors.contains(&format!("\"token\":\"{token}\"")),
            "{colors}"
        );
    }

    // Config show: every key row carries a layer and a source.
    let config = stdout(&fixture.run(&["+show-config", "--format", "json"]));
    assert!(config.contains("\"count\":31"), "{config}");
    assert!(config.contains("\"layer\":\"core-defaults\""));
    assert!(config.contains("\"source\":\"default\""));
}

#[test]
fn invalid_config_is_reported_and_exits_nonzero() {
    let fixture = Fixture::new("invalid");
    fixture.write_config("return { font = }\n");

    for args in [
        vec!["+show-config", "--format", "json"],
        vec!["+validate-config", "--format", "json"],
    ] {
        let out = fixture.run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?} must fail closed");
        let text = stdout(&out);
        assert!(
            text.contains("\"ok\":false") && text.contains("\"code\":\"E_CONFIG_INVALID\""),
            "{args:?} must emit a typed error envelope: {text}"
        );
        assert!(
            stderr(&out).contains("invalid config file"),
            "{args:?} must explain the failure on stderr: {}",
            stderr(&out)
        );
    }

    // Text validation prints diagnostics on stderr only.
    let text = fixture.run(&["+validate-config"]);
    assert_eq!(text.status.code(), Some(2));
    assert!(stdout(&text).is_empty(), "text mode must not emit JSON");
    assert!(stderr(&text).contains("invalid config file"));
}

#[test]
fn show_config_attributes_profile_and_policy_rows() {
    let fixture = Fixture::new("profile-attr");
    fixture.write_config("return {}\n");
    let profile = fixture.write_profile(
        "work",
        "return { font = { family = \"JetBrains Mono\", size = 14 } }\n",
    );
    let out = fixture.run(&["+show-config", "--profile", "work", "--format", "json"]);
    assert_success(&out, "+show-config --profile work");
    let text = stdout(&out);
    assert!(text.contains("\"profile\":\"work\""), "{text}");
    // Compare decoded fields: the JSON writer escapes Windows backslashes, so
    // raw substring matching against `Path::display()` is not portable.
    let profile_row = json_row(&text, "profile");
    assert_eq!(json_string_field(profile_row, "value"), "work");
    assert_eq!(json_string_field(profile_row, "type"), "string");
    assert_eq!(json_string_field(profile_row, "layer"), "profile");
    // Compare canonical path equality, not the rendered path string: Windows
    // may render the profile path in 8.3 short form in the envelope while the
    // fixture holds the long form. Canonicalizing both resolves the spelling
    // difference without weakening the assertion to a file name.
    let source = json_string_field(profile_row, "source");
    let source_path = source
        .strip_prefix("profile: ")
        .unwrap_or_else(|| panic!("profile source must start with 'profile: ': {source}"));
    let decoded = std::fs::canonicalize(source_path)
        .unwrap_or_else(|err| panic!("decoded profile path {source_path:?} must exist: {err}"));
    let expected = std::fs::canonicalize(&profile)
        .unwrap_or_else(|err| panic!("fixture profile path {:?} must exist: {err}", profile));
    assert_eq!(
        decoded, expected,
        "profile row source must canonicalize to the fixture profile path: {profile_row}"
    );
    let contrast_row = json_row(&text, "theme.contrast");
    assert_eq!(json_string_field(contrast_row, "value"), "pass");
    assert_eq!(json_string_field(contrast_row, "layer"), "policy");
    assert_eq!(json_string_field(contrast_row, "source"), "policy");
    assert!(
        text.contains("\"font.size\",\"value\":14"),
        "profile value must reach the projection: {text}"
    );
}

#[test]
fn safe_mode_matches_the_canonical_listing() {
    let fixture = Fixture::new("safe");
    for (alias, canonical) in [("+list-themes", "themes"), ("+list-colors", "colors")] {
        let alias_out = fixture.run(&[alias, "--safe", "--format", "json"]);
        assert_success(&alias_out, alias);
        let canonical_out = fixture.run(&["list", canonical, "--safe", "--format", "json"]);
        assert_success(&canonical_out, canonical);
        assert_eq!(
            result_payload(&stdout(&alias_out)),
            result_payload(&stdout(&canonical_out))
        );
    }
    let extensions = fixture.run(&["+list-extensions", "--safe", "--format", "json"]);
    assert_success(&extensions, "+list-extensions --safe");
    assert!(stdout(&extensions).contains("\"verb\":\"list\""));
    let config = fixture.run(&["+show-config", "--safe", "--format", "json"]);
    assert_success(&config, "+show-config --safe");
    assert!(stdout(&config).contains("\"valid\":true"));
}

#[test]
fn headless_font_discovery_never_panics_without_a_display() {
    let fixture = Fixture::new("fonts-headless");
    // Empty HOME + no DISPLAY/WAYLAND_DISPLAY: a fontless or missing
    // fontconfig host still returns a stable envelope.
    let out = fixture.run(&["+list-fonts", "--format", "json"]);
    assert_success(&out, "+list-fonts");
    let text = stdout(&out);
    assert!(text.contains("\"kind\":\"fonts\""));
    assert!(text.contains("\"supported\":true") || text.contains("\"supported\":false"));
    assert!(
        !stderr(&out).contains("panicked"),
        "font discovery must not panic: {}",
        stderr(&out)
    );
}

#[test]
fn alias_help_is_available_directly_and_through_help() {
    let fixture = Fixture::new("help");
    for alias in [
        "+list-themes",
        "+list-colors",
        "+list-fonts",
        "+list-keybinds",
        "+list-actions",
        "+list-plugins",
        "+list-extensions",
        "+show-config",
        "+validate-config",
        "+edit-config",
    ] {
        let direct = fixture.run(&[alias, "--help"]);
        assert_success(&direct, alias);
        assert!(
            stdout(&direct).contains(&format!("bitty {alias}")),
            "{alias} --help must name the alias: {}",
            stdout(&direct)
        );
        let via_help = fixture.run(&["help", alias]);
        assert_success(&via_help, alias);
        assert_eq!(
            stdout(&direct),
            stdout(&via_help),
            "bitty help {alias} must equal {alias} --help"
        );
    }
}

#[cfg(unix)]
#[test]
fn missing_editor_fails_closed_without_touching_config() {
    let fixture = Fixture::new("missing-editor");
    let original = "return { font = { family = \"JetBrains Mono\", size = 13 } }\n";
    fixture.write_config(original);
    let out = fixture
        .command(&["+edit-config"])
        .env("EDITOR", "bitty-no-such-editor-2464")
        .output()
        .expect("spawn bitty");
    assert_eq!(out.status.code(), Some(1), "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot run editor"),
        "stderr={}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.config_path()).expect("read config"),
        original,
        "a failed editor launch must not rewrite the file"
    );
}

#[cfg(unix)]
#[test]
fn edit_config_rejects_safe_mode_before_touching_the_file() {
    let fixture = Fixture::new("edit-safe");
    let original = "return { font = { family = \"JetBrains Mono\", size = 13 } }\n";
    fixture.write_config(original);
    let out = fixture
        .command(&["+edit-config", "--safe"])
        .env("EDITOR", "true")
        .output()
        .expect("spawn bitty");
    assert_eq!(out.status.code(), Some(2), "stderr={}", stderr(&out));
    assert!(
        stderr(&out).contains("--safe is not supported"),
        "stderr={}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.config_path()).expect("read config"),
        original,
        "rejected --safe must not edit or rewrite the file"
    );

    let json = fixture
        .command(&["+edit-config", "--safe", "--format", "json"])
        .env("EDITOR", "true")
        .output()
        .expect("spawn bitty");
    assert_eq!(json.status.code(), Some(2));
    let text = stdout(&json);
    assert!(text.contains("\"ok\":false"), "{text}");
    assert!(text.contains("\"code\":\"E_SAFE_UNSUPPORTED\""), "{text}");
}

#[cfg(unix)]
#[test]
fn editor_failure_is_exit_one_with_a_typed_envelope() {
    let fixture = Fixture::new("editor-failure");
    fixture.write_config("return {}\n");
    let out = fixture
        .command(&["+edit-config", "--format", "json"])
        .env("EDITOR", "false")
        .output()
        .expect("spawn bitty");
    assert_eq!(
        out.status.code(),
        Some(1),
        "editor failure must normalize to exit 1: stderr={}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("editor 'false' exited with status 1"),
        "stderr={}",
        stderr(&out)
    );
    let text = stdout(&out);
    assert!(text.contains("\"ok\":false"), "{text}");
    assert!(text.contains("\"code\":\"E_EDITOR_FAILED\""), "{text}");
}

#[cfg(unix)]
#[test]
fn edit_config_validates_after_editing_without_corrupting_the_file() {
    let fixture = Fixture::new("edit-validate");
    let broken = "return { font = }\n";
    fixture.write_config(broken);
    let out = fixture
        .command(&["+edit-config"])
        .env("EDITOR", "true")
        .output()
        .expect("spawn bitty");
    assert_eq!(
        out.status.code(),
        Some(2),
        "post-edit validation must fail closed: stderr={}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("configuration is invalid after editing"),
        "stderr={}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.config_path()).expect("read config"),
        broken,
        "post-edit validation must never rewrite the user's bytes"
    );

    // A valid edit exits 0 and JSON reports the editor and validation state.
    fixture.write_config("return { font = { family = \"JetBrains Mono\", size = 13 } }\n");
    let ok = fixture
        .command(&["+edit-config", "--format", "json"])
        .env("EDITOR", "true")
        .output()
        .expect("spawn bitty");
    assert_eq!(ok.status.code(), Some(0), "stderr={}", stderr(&ok));
    let text = stdout(&ok);
    assert!(text.contains("\"valid\":true"), "{text}");
    assert!(text.contains("\"editor\":\"true\""), "{text}");
}

#[test]
fn completion_offers_every_alias() {
    let fixture = Fixture::new("completion");
    let out = fixture.run(&["completion", "bash"]);
    assert_success(&out, "completion bash");
    let script = stdout(&out);
    for alias in [
        "+list-themes",
        "+list-colors",
        "+list-fonts",
        "+list-keybinds",
        "+list-actions",
        "+list-plugins",
        "+list-extensions",
        "+show-config",
        "+validate-config",
        "+edit-config",
    ] {
        assert!(script.contains(alias), "bash completion must offer {alias}");
    }
    assert!(script.contains("show validate"));
}
