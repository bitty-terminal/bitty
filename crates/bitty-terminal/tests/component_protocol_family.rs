//! Per-component-family wire protocol selection (bitty#2777).
//!
//! `bitty-component.toml` declares the protocol range a component speaks;
//! the install/inventory gate must match it against the host seam that owns
//! the component family, not against one global wire constant:
//!
//! - `graphics`, `net`, `storage`, and `a11y` stay on the shared v1 wire;
//! - `execution` speaks the `bitty-execution` v2 codec;
//! - an unlisted name keeps the strict v1 default.
//!
//! These tests drive the built `bitty` binary with isolated XDG roots, so the
//! developer's real extensions are never read or written, and installation
//! executes no component code.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-component-family-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn run_in(home: &Path, system: &Path, args: &[&str]) -> Output {
    Command::new(BITTY_BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("HOME", home)
        .env("BITTY_SYSTEM_COMPONENTS_DIR", system)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_COMPONENTS_DIR")
        .env_remove("BITTY_COMPONENT_STATE_DIR")
        .env_remove("BITTY_PLUGIN_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("run {BITTY_BIN:?} {args:?}: {error}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn user_root(home: &Path) -> PathBuf {
    home.join("bitty").join("extensions")
}

/// Write a local-path source directory holding a descriptor and executable.
fn write_source_dir(dir: &Path, name: &str, version: &str, protocol: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("source dir");
    let executable = format!("bitty-{name}");
    let file_name = format!("{executable}{}", std::env::consts::EXE_SUFFIX);
    std::fs::write(dir.join(&file_name), format!("{name}-{version}-bytes")).expect("executable");
    std::fs::write(
        dir.join("bitty-component.toml"),
        format!(
            "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = {protocol}\nexecutable = \"{executable}\"\n"
        ),
    )
    .expect("descriptor");
    dir.to_owned()
}

fn add(home: &Path, system: &Path, source: &Path) -> Output {
    run_in(
        home,
        system,
        &["extension", "add", &source.display().to_string()],
    )
}

#[test]
fn v2_execution_descriptor_is_accepted_by_the_v2_family() {
    let home = scratch_dir("execution-v2");
    let system = home.join("system");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "execution", "0.0.1", "[2, 2]");

    let output = add(&home, &system, &source);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        user_root(&home)
            .join("execution/0.0.1/bitty-component.toml")
            .is_file(),
        "the v2 descriptor must be staged"
    );

    let output = run_in(&home, &system, &["extension", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"name\":\"execution\""), "{text}");
    assert!(text.contains("\"protocol_min\":2"), "{text}");
    assert!(text.contains("\"protocol_max\":2"), "{text}");
    assert!(
        text.contains("\"compatible\":true"),
        "the v2 family seam must report the install compatible: {text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn shared_wire_families_keep_requiring_v1() {
    let home = scratch_dir("v1-families");
    let system = home.join("system");
    std::fs::create_dir_all(&system).expect("system dir");

    // Every v1 family accepts its declared v1 descriptor.
    for name in ["graphics", "net", "storage", "a11y"] {
        let source = write_source_dir(
            &home.join(format!("source-{name}")),
            name,
            "0.0.1",
            "[1, 1]",
        );
        let output = add(&home, &system, &source);
        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
    }
    // The same families refuse a v2-only descriptor: the shared wire is not
    // a fallback for a range the seam cannot serve.
    for name in ["graphics", "net", "storage", "a11y"] {
        let source = write_source_dir(
            &home.join(format!("source-v2-{name}")),
            name,
            "0.0.2",
            "[2, 2]",
        );
        let output = add(&home, &system, &source);
        assert_eq!(
            output.status.code(),
            Some(4),
            "{name} must refuse v2: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("network-wire-v1"),
            "{name} refusal must name the v1 seam: {}",
            stderr(&output)
        );
        assert!(
            !user_root(&home).join(name).join("0.0.2").exists(),
            "{name} ABI mismatch must not stage files"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn descriptor_host_mismatch_is_refused_for_execution() {
    let home = scratch_dir("execution-mismatch");
    let system = home.join("system");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "execution", "0.0.1", "[1, 1]");

    let output = add(&home, &system, &source);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stderr(&output);
    assert!(text.contains("execution-v2"), "{text}");
    assert!(text.contains("[2, 2]"), "{text}");
    assert!(
        !user_root(&home).join("execution").exists(),
        "descriptor/host mismatch must not stage files"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn unknown_family_keeps_the_strict_v1_default() {
    let home = scratch_dir("unknown-family");
    let system = home.join("system");
    std::fs::create_dir_all(&system).expect("system dir");

    // Unknown name + v2-only range fails closed against the default seam.
    let source = write_source_dir(&home.join("source"), "mystery", "0.0.1", "[2, 2]");
    let output = add(&home, &system, &source);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("network-wire-v1"),
        "{}",
        stderr(&output)
    );
    assert!(!user_root(&home).join("mystery").exists());

    // Unknown name + v1 range is accepted; the default is strict, not
    // permissive (it does not widen to v2 or to an arbitrary version).
    let source = write_source_dir(&home.join("source-v1"), "mystery", "0.0.1", "[1, 1]");
    let output = add(&home, &system, &source);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // An unlisted protocol version fails closed as well.
    let source = write_source_dir(&home.join("source-v3"), "mystery", "0.0.2", "[3, 3]");
    let output = add(&home, &system, &source);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bare_execution_executable_stages_the_v2_family_range() {
    let home = scratch_dir("bare-execution");
    let system = home.join("system");
    std::fs::create_dir_all(&system).expect("system dir");
    let executable_name = format!("bitty-execution{}", std::env::consts::EXE_SUFFIX);
    let executable = home.join(executable_name);
    std::fs::write(&executable, "bare-v2-bytes").expect("executable");

    let output = run_in(
        &home,
        &system,
        &[
            "extension",
            "add",
            &executable.display().to_string(),
            "--version",
            "0.0.1",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let descriptor =
        std::fs::read_to_string(user_root(&home).join("execution/0.0.1/bitty-component.toml"))
            .expect("staged descriptor");
    assert!(
        descriptor.contains("protocol = [2, 2]"),
        "bare staging must declare the v2 family range: {descriptor}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
