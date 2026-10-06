//! `bitty component` end-to-end dispatch proofs (issue #1651).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/components/` wins over system
//! (`BITTY_SYSTEM_COMPONENTS_DIR` in tests, `/usr/lib/bitty/components/`
//! in production). `add` stages from local paths only (no network fetch);
//! every spawn re-verifies ABI compat. These tests drive the built `bitty`
//! binary via `CARGO_BIN_EXE_bitty` with isolated XDG roots, so the
//! developer's real components are never touched. `component` dispatches
//! before config load and GUI startup: no display, no instance, no component
//! code is ever executed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated home for one test.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-component-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run the binary with isolated XDG roots and an isolated system tier.
///
/// Both XDG roots are pinned into the scratch home, the system tier into
/// `<home>/system-components`, and the development overrides cleared, so a
/// test can never read or write the developer's real components.
fn run_in(home: &Path, system: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
        .args(args)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("HOME", home)
        .env("BITTY_SYSTEM_COMPONENTS_DIR", system)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_COMPONENTS_DIR")
        .env_remove("BITTY_PLUGIN_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    command
        .output()
        .unwrap_or_else(|err| panic!("run {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn user_root(home: &Path) -> PathBuf {
    home.join("bitty").join("components")
}

/// Write a source directory holding `bitty-component.toml` (no `sha256`,
/// per D6 local-path form) plus the executable.
///
/// The descriptor stores the logical executable name (`bitty-<name>`); the
/// file on disk carries the platform suffix (`bitty-<name>.exe` on Windows),
/// mirroring the production install layout.
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

/// Install a component directly into a tier root (bypasses the CLI, for
/// system-tier fixtures the CLI never writes).
///
/// Like `write_source_dir`, the descriptor keeps the logical executable name
/// while the file on disk uses the platform suffix.
fn install_into(root: &Path, name: &str, version: &str, protocol: &str) {
    let version_dir = root.join(name).join(version);
    std::fs::create_dir_all(&version_dir).expect("version dir");
    let executable = format!("bitty-{name}");
    let file_name = format!("{executable}{}", std::env::consts::EXE_SUFFIX);
    let executable_path = version_dir.join(&file_name);
    std::fs::write(&executable_path, format!("{name}-{version}-bytes")).expect("executable");
    let bytes = std::fs::read(&executable_path).expect("read");
    let digest = bitty_package::integrity::sha256_hex(&bytes);
    std::fs::write(
        version_dir.join("bitty-component.toml"),
        format!(
            "[component]\nname = \"{name}\"\nversion = \"{version}\"\nprotocol = {protocol}\nexecutable = \"{executable}\"\nsha256 = \"{digest}\"\n"
        ),
    )
    .expect("descriptor");
    std::fs::write(root.join(name).join("current"), format!("{version}\n")).expect("current");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
            .expect("mode");
    }
}

#[test]
fn component_list_is_empty_and_local() {
    let home = scratch_dir("empty");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["component", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("(no components)"),
        "{}",
        stdout(&output)
    );
    // No user tier is created by a read-only list.
    assert!(!user_root(&home).join("net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_list_json_is_a_versioned_envelope() {
    let home = scratch_dir("json");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["component", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"v\":1"), "{text}");
    assert!(text.contains("\"command\":\"component\""), "{text}");
    assert!(text.contains("\"verb\":\"list\""), "{text}");
    assert!(text.contains("\"count\":0"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_add_list_remove_lifecycle() {
    let home = scratch_dir("lifecycle");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let source = source.display().to_string();

    let output = run_in(&home, &system, &["component", "add", &source]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    assert!(
        user_root(&home)
            .join("net/0.0.1/bitty-component.toml")
            .is_file(),
        "descriptor must be staged"
    );
    assert!(
        user_root(&home).join("net/current").is_file(),
        "current must be staged"
    );

    let output = run_in(&home, &system, &["component", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("net"), "{text}");
    assert!(text.contains("0.0.1"), "{text}");
    assert!(text.contains("user"), "{text}");
    assert!(text.contains('*'), "{text}");

    let output = run_in(&home, &system, &["component", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"name\":\"net\""), "{text}");
    assert!(text.contains("\"active_version\":\"0.0.1\""), "{text}");
    assert!(text.contains("\"active_source\":\"user\""), "{text}");

    let output = run_in(&home, &system, &["component", "remove", "net", "0.0.1"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!user_root(&home).join("net/0.0.1").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_add_bare_executable_needs_version() {
    let home = scratch_dir("bare");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let executable = home.join("bitty-net");
    std::fs::write(&executable, "bare-bytes").expect("executable");

    // Missing --version is a usage error (exit 2), no files staged.
    let output = run_in(
        &home,
        &system,
        &["component", "add", &executable.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(!user_root(&home).join("net").exists());

    let output = run_in(
        &home,
        &system,
        &[
            "component",
            "add",
            &executable.display().to_string(),
            "--version",
            "0.0.1",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        user_root(&home)
            .join("net/0.0.1/bitty-component.toml")
            .is_file()
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_add_rejects_abi_mismatch_without_staging() {
    let home = scratch_dir("abi");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[99, 99]");
    let output = run_in(
        &home,
        &system,
        &["component", "add", &source.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("protocol"),
        "ABI denial must name the protocol range: {}",
        stderr(&output)
    );
    assert!(
        !user_root(&home).join("net").exists(),
        "ABI mismatch must not stage files"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_add_rejects_remote_source() {
    let home = scratch_dir("remote");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(
        &home,
        &system,
        &["component", "add", "https://example.com/net.tar.gz"],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not implemented") || stderr(&output).contains("local"),
        "{}",
        stderr(&output)
    );
    assert!(!user_root(&home).join("net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_missing_soft_fails_with_actionable_error() {
    let home = scratch_dir("missing");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["component", "remove", "net"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("bitty component add"),
        "missing component must name the install command: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_user_overrides_system_on_collision() {
    let home = scratch_dir("override");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    // System tier holds 0.0.1 + 0.0.2; the user adds 0.0.2 (same version).
    install_into(&system, "net", "0.0.1", "[1, 1]");
    install_into(&system, "net", "0.0.2", "[1, 1]");
    let source = write_source_dir(&home.join("source"), "net", "0.0.2", "[1, 1]");
    let output = run_in(
        &home,
        &system,
        &["component", "add", &source.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let output = run_in(&home, &system, &["component", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    // Both versions are visible; the colliding 0.0.2 resolves from the user tier.
    assert!(text.contains("0.0.1"), "{text}");
    assert!(text.contains("0.0.2"), "{text}");
    assert!(text.contains("user"), "{text}");
    assert!(text.contains("system"), "{text}");

    let output = run_in(&home, &system, &["component", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"active_source\":\"user\""), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_remove_never_touches_system_tier() {
    let home = scratch_dir("system-only");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    install_into(&system, "net", "0.0.1", "[1, 1]");

    // System-only remove fails closed with a package-manager diagnostic and
    // leaves the system tier intact (no root required, none used).
    let output = run_in(&home, &system, &["component", "remove", "net"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("system-wide") || stderr(&output).contains("package manager"),
        "{}",
        stderr(&output)
    );
    assert!(system.join("net/0.0.1/bitty-component.toml").is_file());
    assert!(!user_root(&home).join("net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_help_and_usage_have_stable_exit_codes() {
    let home = scratch_dir("help");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    let output = run_in(&home, &system, &["component", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("usage: bitty component"),
        "{}",
        stdout(&output)
    );

    let output = run_in(&home, &system, &["component", "frobnicate"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("usage: bitty component"),
        "{}",
        stderr(&output)
    );

    let output = run_in(&home, &system, &["component", "add"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    let output = run_in(&home, &system, &["component", "remove", "Not-A-Name!"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_add_publishes_atomically_without_temp_litter() {
    let home = scratch_dir("atomic");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let source_display = source.display().to_string();

    let output = run_in(&home, &system, &["component", "add", &source_display]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // No temp siblings survive a successful publish, at any depth.
    let mut temps = Vec::new();
    let mut stack = vec![user_root(&home).join("net")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".tmp-") || name.contains(".tmp.") {
                temps.push(entry.path());
            } else if entry.path().is_dir() {
                stack.push(entry.path());
            }
        }
    }
    assert!(temps.is_empty(), "temp litter survived publish: {temps:?}");

    // All three destinations are fully published and executable.
    let version_dir = user_root(&home).join("net/0.0.1");
    assert!(version_dir.join("bitty-component.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(user_root(&home).join("net/current")).expect("current"),
        "0.0.1\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let executable = version_dir.join(format!("bitty-net{}", std::env::consts::EXE_SUFFIX));
        let mode = std::fs::metadata(&executable)
            .expect("mode")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "published executable must be 0755");
    }

    // Re-adding the same version with different bytes fails closed with the
    // digest diagnostic and leaves the installed bytes untouched.
    let before =
        std::fs::read(version_dir.join(format!("bitty-net{}", std::env::consts::EXE_SUFFIX)))
            .expect("installed bytes");
    let source2 = write_source_dir(&home.join("source2"), "net", "0.0.1", "[1, 1]");
    std::fs::write(
        source2.join(format!("bitty-net{}", std::env::consts::EXE_SUFFIX)),
        b"different-bytes",
    )
    .expect("mutate source");
    let output = run_in(
        &home,
        &system,
        &["component", "add", &source2.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("different digest"),
        "{}",
        stderr(&output)
    );
    let after =
        std::fs::read(version_dir.join(format!("bitty-net{}", std::env::consts::EXE_SUFFIX)))
            .expect("installed bytes");
    assert_eq!(
        before, after,
        "failed re-add must not overwrite the install"
    );
    let _ = std::fs::remove_dir_all(&home);
}
