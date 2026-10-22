//! `bitty extension` end-to-end dispatch proofs (issue #1651, verb renamed
//! from `bitty component` by issue #1959).
//!
//! Search priority: user `$XDG_DATA_HOME/bitty/extensions/` wins over system
//! (`BITTY_SYSTEM_COMPONENTS_DIR` in tests, `/usr/lib/bitty/extensions/`
//! in production). `add` stages from local paths only (no network fetch);
//! every spawn re-verifies ABI compat. These tests drive the built `bitty`
//! binary via `CARGO_BIN_EXE_bitty` with isolated XDG roots, so the
//! developer's real components are never touched. `extension` dispatches
//! before config load and GUI startup: no display, no instance, no component
//! code is ever executed.
//!
//! The hidden `component` alias shares these handlers byte-for-byte on
//! stdout and adds only a stderr deprecation note (kept at least through
//! 0.1.0): the pre-existing `component ...` cases below double as alias
//! proofs, and `component_alias_works_and_warns` pins the warning.

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
        .stdin(Stdio::null());
    command
        .output()
        .unwrap_or_else(|err| panic!("run {BITTY_BIN:?} {args:?}: {err}"))
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Run the binary like [`run_in`] plus extra child-environment entries.
///
/// `extra_env` entries override the inherited environment (used to poison
/// secrets or to point `PATH` at an empty dir so `curl`/`tar` resolve to
/// nothing). The binary itself is invoked by absolute path, so a scrubbed
/// `PATH` only blinds its children, never the test harness.
fn run_with_env(home: &Path, system: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BITTY_BIN);
    command
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
        .stdin(Stdio::null());
    for (name, value) in extra_env {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|err| panic!("run {BITTY_BIN:?} {args:?}: {err}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn user_root(home: &Path) -> PathBuf {
    home.join("bitty").join("extensions")
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
        stdout(&output).contains("(no extensions)"),
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
fn component_install_needs_consent_and_an_exact_version_when_pinned() {
    let home = scratch_dir("install-version");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    // No `--version` resolves the catalog latest, but consent comes first:
    // empty stdin aborts (exit 1) before any fetch or staging.
    let output = run_in(&home, &system, &["component", "install", "net"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not approved") || stderr(&output).contains("aborted"),
        "{}",
        stderr(&output)
    );
    assert!(!user_root(&home).join("net").exists());
    // Non-`X.Y.Z` pins fail before any fetch (usage, not integrity).
    let output = run_in(
        &home,
        &system,
        &["component", "install", "net", "--version", "1.0-alpha"],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(!user_root(&home).join("net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_install_rejects_hostile_names_without_fetch() {
    let home = scratch_dir("install-hostile");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    // Hostile names fail while building the allowlisted URL, before any
    // transport contact: a regression to fetch-first would attempt real
    // network access and surface a different exit code.
    for hostile in [
        "https://evil.example/net.tar.gz",
        "../evil",
        "net;evil",
        "net|evil",
    ] {
        let output = run_in(
            &home,
            &system,
            &["component", "install", hostile, "--version", "0.0.23"],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{hostile}: {}",
            stderr(&output)
        );
    }
    assert!(!user_root(&home).join("net").exists());
    assert!(!user_root(&home).join("evil").exists());
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
        stderr(&output).contains("bitty extension add"),
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
        stdout(&output).contains("usage: bitty extension"),
        "{}",
        stdout(&output)
    );

    let output = run_in(&home, &system, &["component", "frobnicate"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    // #2381: errors stay targeted; full usage lives behind --help.
    assert!(
        stderr(&output).contains("bitty extension --help"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("usage: bitty extension"),
        "usage must not be dumped on errors: {}",
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

/// Unix-only: hiding `curl`/`tar` through `PATH` cannot work on Windows.
/// Windows `CreateProcess` resolves tools through a fixed order (application
/// directory, then `C:\Windows\System32`, then the Windows directory, and
/// only then `PATH`), so `C:\Windows\System32\curl.exe` (and `tar.exe`)
/// always resolve no matter how `PATH` is scrubbed. The fail-closed
/// diagnostic itself stays covered on every platform by the
/// `missing_tool_diagnostics_*` and `tool_version_output_against_absent_*`
/// unit tests, which use a guaranteed-absent tool name instead of `PATH`.
#[cfg(unix)]
#[test]
fn component_install_absent_helpers_fails_closed_with_manual_path() {
    let home = scratch_dir("absent-helpers");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    // A PATH with no helpers: the tar presence preflight fires first (before
    // any fetch or staging), so the diagnostic names `tar`. `--yes` skips
    // consent so the failure lands in the fetch environment, not the prompt.
    let empty_path = home.join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty path dir");
    let output = run_with_env(
        &home,
        &system,
        &[
            "component",
            "install",
            "net",
            "--version",
            "0.0.23",
            "--yes",
        ],
        &[("PATH", &empty_path.display().to_string())],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("`tar` not found"),
        "absent tar must name the tool: {diagnostic}"
    );
    assert!(
        diagnostic.contains("$XDG_DATA_HOME/bitty/extensions/"),
        "diagnostic must point at manual placement: {diagnostic}"
    );
    assert!(
        diagnostic.contains("nothing was fetched or staged"),
        "diagnostic must promise no staging: {diagnostic}"
    );
    assert!(
        !user_root(&home).join("net").exists(),
        "absent helpers must stage nothing"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_install_with_hostile_env_still_fails_closed() {
    let home = scratch_dir("hostile-env");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let poisoned: &[(&str, &str)] = &[
        ("BITTY_CTX1112_GITHUB_TOKEN", "poison-token"),
        ("BITTY_CTX1112_AWS_SECRET_ACCESS_KEY", "poison-secret"),
        ("BITTY_CTX1112_API_KEY", "poison-key"),
    ];
    // Hostile names still fail at URL construction (exit 2) with zero spawn,
    // even under a poisoned environment: validation precedes any fetch, so
    // no secret can reach a child that is never spawned. Portable: no `PATH`
    // games, so this half runs on every platform.
    let output = run_with_env(
        &home,
        &system,
        &["component", "install", "../evil", "--version", "0.0.23"],
        poisoned,
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&home);
}

/// Unix-only: the poisoned-environment plus scrubbed-`PATH` half of the
/// hostile proof (see the `System32` note on
/// `component_install_absent_helpers_fails_closed_with_manual_path`).
#[cfg(unix)]
#[test]
fn component_install_with_hostile_env_and_absent_helpers_fails_closed() {
    let home = scratch_dir("hostile-env-nocurl");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let poisoned: &[(&str, &str)] = &[
        ("BITTY_CTX1112_GITHUB_TOKEN", "poison-token"),
        ("BITTY_CTX1112_AWS_SECRET_ACCESS_KEY", "poison-secret"),
        ("BITTY_CTX1112_API_KEY", "poison-key"),
    ];
    // With no helpers on PATH, the poisoned environment still lands in the
    // tar-missing diagnostic (exit 1, tar preflight fires first), never in
    // a child.
    let empty_path = home.join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty path dir");
    let empty_path_str = empty_path.display().to_string();
    let mut env: Vec<(&str, &str)> = poisoned.to_vec();
    env.push(("PATH", &empty_path_str));
    let output = run_with_env(
        &home,
        &system,
        &[
            "component",
            "install",
            "net",
            "--version",
            "0.0.23",
            "--yes",
        ],
        &env,
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("`tar` not found"),
        "{}",
        stderr(&output)
    );
    assert!(
        !user_root(&home).join("net").exists(),
        "hostile env must stage nothing"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_help_documents_fetch_environment() {
    let home = scratch_dir("fetch-help");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["component", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    // Curl floor: preflight + minimum version.
    assert!(text.contains("curl --version"), "{text}");
    assert!(text.contains("7.68.0"), "{text}");
    // Absent/too-old diagnostic: manual placement path + exit code.
    assert!(text.contains("$XDG_DATA_HOME/bitty/extensions/"), "{text}");
    // CA + proxy posture pin plus the no-secrets audit.
    assert!(text.contains("system CA store"), "{text}");
    assert!(text.contains("proxy"), "{text}");
    assert!(text.contains("*_TOKEN"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_list_is_empty_and_local() {
    // Primary verb (issue #1959): same empty-list behavior as the alias,
    // with no deprecation note on stderr.
    let home = scratch_dir("ext-empty");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("(no extensions)"),
        "{}",
        stdout(&output)
    );
    assert!(
        !stderr(&output).contains("deprecated"),
        "canonical verb must not warn: {}",
        stderr(&output)
    );
    assert!(!user_root(&home).join("net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_add_list_remove_lifecycle() {
    // Primary verb lifecycle: add stages, list shows, remove drops.
    let home = scratch_dir("ext-lifecycle");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let source = source.display().to_string();

    let output = run_in(&home, &system, &["extension", "add", &source]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    assert!(
        user_root(&home)
            .join("net/0.0.1/bitty-component.toml")
            .is_file(),
        "descriptor must be staged"
    );

    let output = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("net"), "{text}");
    assert!(text.contains("0.0.1"), "{text}");

    let output = run_in(&home, &system, &["extension", "list", "--format", "json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("\"command\":\"component\""), "{text}");
    assert!(text.contains("\"verb\":\"list\""), "{text}");

    let output = run_in(&home, &system, &["extension", "remove", "net", "0.0.1"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!user_root(&home).join("net/0.0.1").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_help_and_usage_name_new_verb() {
    let home = scratch_dir("ext-help");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    let output = run_in(&home, &system, &["extension", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("usage: bitty extension"),
        "{}",
        stdout(&output)
    );
    assert!(
        !stdout(&output).contains("bitty component list"),
        "help must not name the old verb: {}",
        stdout(&output)
    );

    let output = run_in(&home, &system, &["extension", "frobnicate"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("bitty extension --help"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("usage: bitty extension"),
        "usage must not be dumped on errors: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn component_alias_works_and_warns() {
    // Hidden deprecated alias (issue #1959, kept at least through 0.1.0):
    // full behavior plus a stderr deprecation note pointing at `extension`.
    // Stdout stays byte-identical to the canonical verb.
    let home = scratch_dir("alias-warns");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    let via_alias = run_in(&home, &system, &["component", "list"]);
    let via_canonical = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(via_alias.status.code(), Some(0), "{}", stderr(&via_alias));
    assert_eq!(stdout(&via_alias), stdout(&via_canonical));
    let warning = stderr(&via_alias);
    assert!(
        warning.contains("deprecated") && warning.contains("extension"),
        "alias must warn toward the new verb: {warning}"
    );
    assert!(
        !stderr(&via_canonical).contains("deprecated"),
        "canonical verb must not warn: {}",
        stderr(&via_canonical)
    );

    // Alias help prints the canonical text (stdout) with the note on stderr.
    let output = run_in(&home, &system, &["component", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("usage: bitty extension"),
        "{}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("deprecated"),
        "alias help must warn: {}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_show_source_prints_pinned_bytes() {
    // Issue #1986: `show-source` prints the exact pinned executable bytes to
    // stdout (provenance on stderr so stdout stays byte-identical), version
    // defaulting to the active `current` pointer.
    let home = scratch_dir("ext-show-source");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let source = source.display().to_string();

    let output = run_in(&home, &system, &["extension", "add", &source]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let output = run_in(&home, &system, &["extension", "show-source", "net"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "net-0.0.1-bytes");
    let provenance = stderr(&output);
    assert!(
        provenance.contains("show-source") && provenance.contains("net"),
        "provenance must name the verb and component: {provenance}"
    );

    // Explicit version agrees byte-for-byte.
    let output = run_in(
        &home,
        &system,
        &["extension", "show-source", "net", "0.0.1"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "net-0.0.1-bytes");

    // Missing installs fail closed (exit 4) with nothing on stdout.
    let output = run_in(&home, &system, &["extension", "show-source", "missing"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    let output = run_in(
        &home,
        &system,
        &["extension", "show-source", "net", "9.9.9"],
    );
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_repo_prefix_alias_resolves_without_renaming_storage() {
    // #2381: `bitty-net` is an alias for the stored component `net` in
    // list/install/remove/show-source; directories and pins never change.
    let home = scratch_dir("alias-lifecycle");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let output = run_in(
        &home,
        &system,
        &["extension", "add", &source.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(user_root(&home).join("net/0.0.1").is_dir());
    assert!(!user_root(&home).join("bitty-net").exists());

    // list accepts the alias as a filter.
    let output = run_in(&home, &system, &["extension", "list", "bitty-net"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("net") && text.contains("0.0.1"), "{text}");

    // show-source prints the pinned bytes through the alias.
    let output = run_in(&home, &system, &["extension", "show-source", "bitty-net"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "net-0.0.1-bytes");

    // remove drops the stored `net` path, never a `bitty-net` path.
    let output = run_in(&home, &system, &["extension", "remove", "bitty-net"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!user_root(&home).join("net").exists());
    assert!(!user_root(&home).join("bitty-net").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_alias_ambiguity_fails_closed() {
    // Both `net` and a literal `bitty-net` installed: the alias must not
    // guess which one the operator meant.
    let home = scratch_dir("alias-ambiguous");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    install_into(&user_root(&home), "net", "0.0.1", "[1, 1]");
    install_into(&user_root(&home), "bitty-net", "0.0.1", "[1, 1]");
    let output = run_in(&home, &system, &["extension", "remove", "bitty-net"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("ambiguous"), "{diagnostic}");
    assert!(
        diagnostic.contains("net") && diagnostic.contains("bitty-net"),
        "{diagnostic}"
    );
    // Nothing was removed.
    assert!(user_root(&home).join("net/0.0.1").is_dir());
    assert!(user_root(&home).join("bitty-net/0.0.1").is_dir());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_unknown_name_suggests_closest_known_command() {
    let home = scratch_dir("unknown-name-hint");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    // Typo of a built-in extension: closest match plus the exact command.
    let output = run_in(&home, &system, &["extension", "remove", "grahpics"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("did you mean 'graphics'"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("bitty extension remove graphics"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("usage: bitty extension"),
        "error must not dump usage: {diagnostic}"
    );

    // A known but uninstalled name points at install, not at a typo guess.
    let output = run_in(&home, &system, &["extension", "show-source", "net"]);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("'net' is not installed")
            && diagnostic.contains("bitty extension install net"),
        "{diagnostic}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_install_without_version_prompts_before_network() {
    // Bare install resolves the catalog latest, but consent precedes every
    // transport contact: empty stdin aborts with nothing fetched or staged.
    let home = scratch_dir("latest-consent");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    // A scrubbed PATH would surface a tool-missing error if anything spawned;
    // the consent abort must win first.
    let empty_path = home.join("empty-path");
    std::fs::create_dir_all(&empty_path).expect("empty path");
    let output = run_with_env(
        &home,
        &system,
        &["extension", "install", "bitty-graphics"],
        &[("PATH", &empty_path.display().to_string())],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("aborted") || stderr(&output).contains("not approved"),
        "{}",
        stderr(&output)
    );
    // The alias resolved to `graphics` before the prompt: the install path
    // names the stored component, never `bitty-graphics`.
    let prompt = stdout(&output);
    assert!(prompt.contains("latest 'graphics' release"), "{prompt}");
    assert!(!prompt.contains("bitty-graphics"), "{prompt}");
    assert!(!user_root(&home).join("graphics").exists());
    assert!(!user_root(&home).join("bitty-graphics").exists());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_help_documents_latest_pinning_and_aliases() {
    let home = scratch_dir("help-latest");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let output = run_in(&home, &system, &["extension", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("bitty extension install graphics --version 0.0.24"),
        "{text}"
    );
    assert!(
        text.contains("bitty extension install net --version 0.0.1"),
        "{text}"
    );
    assert!(text.contains("latest"), "{text}");
    assert!(text.contains("bitty-graphics"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_show_source_is_visible_in_help() {
    // Issue #1986: `show-source` is a visible verb (unlike the hidden
    // `component` alias, which never appears in help or completions).
    let home = scratch_dir("ext-show-source-help");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    let output = run_in(&home, &system, &["extension", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("show-source"),
        "extension help must document show-source:\n{}",
        stdout(&output)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Activation column of the `net` row in a table render (`None` if absent).
fn activation_column(text: &str) -> Option<String> {
    text.lines()
        .find(|line| line.starts_with("net "))
        .and_then(|line| line.split_whitespace().nth(5).map(str::to_owned))
}

#[test]
fn extension_list_reports_recorded_activation_state() {
    let home = scratch_dir("activation");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    let source = write_source_dir(&home.join("source"), "net", "0.0.1", "[1, 1]");
    let output = run_in(
        &home,
        &system,
        &["extension", "add", &source.display().to_string()],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    let component_dir = user_root(&home).join("net");
    // Installed through `add`, never activated: `installed`, never `active`.
    let output = run_in(&home, &system, &["extension", "list"]);
    let text = stdout(&output);
    assert!(text.contains("ACTIVATION"), "{text}");
    assert_eq!(activation_column(&text).as_deref(), Some("installed"));
    let output = run_in(&home, &system, &["extension", "list", "--format", "json"]);
    let text = stdout(&output);
    assert!(text.contains("\"activation\":\"installed\""), "{text}");
    assert!(text.contains("\"activation_class\":null"), "{text}");

    // Active: exactly what the broker records after a spawn.
    std::fs::write(component_dir.join("activation"), "v1 active\n").expect("active record");
    let output = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(
        activation_column(&stdout(&output)).as_deref(),
        Some("active")
    );
    let output = run_in(&home, &system, &["extension", "list", "--format", "json"]);
    let text = stdout(&output);
    assert!(text.contains("\"activation\":\"active\""), "{text}");
    assert!(text.contains("\"activation_class\":null"), "{text}");

    // Denied: the named error class travels with the label.
    std::fs::write(
        component_dir.join("activation"),
        "v1 denied:capability_denied\n",
    )
    .expect("denied record");
    let output = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(
        activation_column(&stdout(&output)).as_deref(),
        Some("denied:capability_denied")
    );
    let output = run_in(&home, &system, &["extension", "list", "--format", "json"]);
    let text = stdout(&output);
    assert!(
        text.contains("\"activation\":\"denied:capability_denied\""),
        "{text}"
    );
    assert!(
        text.contains("\"activation_class\":\"capability_denied\""),
        "{text}"
    );

    // Crashed and stopped are distinct labels, not a generic "inactive".
    for (record, label) in [("v1 crashed\n", "crashed"), ("v1 stopped\n", "stopped")] {
        std::fs::write(component_dir.join("activation"), record).expect("record");
        let output = run_in(&home, &system, &["extension", "list"]);
        assert_eq!(activation_column(&stdout(&output)).as_deref(), Some(label));
    }

    // A corrupt or versionless record never claims an activation.
    for bad in ["active\n", "", "v1 denied:made_up\n"] {
        std::fs::write(component_dir.join("activation"), bad).expect("bad record");
        let output = run_in(&home, &system, &["extension", "list"]);
        assert_eq!(
            activation_column(&stdout(&output)).as_deref(),
            Some("installed"),
            "{bad:?} must not claim a state"
        );
    }

    // Removing the last version drops the projection with the component
    // (an activation record must not keep a removed install visible).
    let output = run_in(&home, &system, &["extension", "remove", "net", "0.0.1"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!component_dir.exists(), "component dir must be pruned");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn extension_list_prints_state_record_once_on_the_active_version() {
    let home = scratch_dir("activation-state-per-version");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");
    for version in ["0.0.1", "0.0.2"] {
        let source = write_source_dir(
            &home.join(format!("source-{version}")),
            "net",
            version,
            "[1, 1]",
        );
        let output = run_in(
            &home,
            &system,
            &["extension", "add", &source.display().to_string()],
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    let state_dir = home
        .join("state")
        .join("bitty")
        .join("extensions")
        .join("net");
    std::fs::create_dir_all(&state_dir).expect("state dir");
    std::fs::write(state_dir.join("activation"), "v1 active\n").expect("state record");

    let output = run_in(&home, &system, &["extension", "list"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("net "))
        .collect();
    assert_eq!(rows.len(), 2, "{text}");
    let column = |version: &str| {
        rows.iter()
            .find(|line| line.split_whitespace().nth(1) == Some(version))
            .and_then(|line| line.split_whitespace().nth(5))
            .map(str::to_owned)
    };
    assert_ne!(column("0.0.1").as_deref(), Some("active"), "{text}");
    assert_eq!(column("0.0.2").as_deref(), Some("active"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

/// Terminology golden (issue #2440): the installable artifact is an
/// "extension" in every user-facing surface. "component" survives only for
/// the internal seam (descriptor file name, module paths) and the stable
/// machine envelope, never in help or table prose.
#[test]
fn extension_help_and_list_use_extension_terminology() {
    let home = scratch_dir("terminology");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    let output = run_in(&home, &system, &["extension", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let help = stdout(&output);
    assert!(
        help.contains("Show installed extensions across both tiers"),
        "{help}"
    );
    for forbidden in [
        "installed components",
        "component name",
        "component error",
        "component execution",
        "optional components",
        "component code",
        "user component root",
        "per component",
    ] {
        assert!(
            !help.contains(forbidden),
            "help still mixes terminology with {forbidden:?}:\n{help}"
        );
    }
    // The internal descriptor file name is a contract, not user prose.
    assert!(help.contains("bitty-component.toml"), "{help}");

    let list = stdout(&run_in(&home, &system, &["extension", "list"]));
    assert!(list.contains("(no extensions)"), "{list}");
    assert!(!list.contains("component"), "{list}");

    // The JSON envelope keeps its documented stable key names so existing
    // machine consumers do not break on a prose rename.
    let json = stdout(&run_in(
        &home,
        &system,
        &["extension", "list", "--format", "json"],
    ));
    assert!(json.contains("\"command\":\"component\""), "{json}");
    assert!(json.contains("\"components\":["), "{json}");
    let _ = std::fs::remove_dir_all(&home);
}
