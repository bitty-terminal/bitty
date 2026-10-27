//! bitty#2606 / CTX-1616: the `bittyd` one-way dependency direction.
//!
//! The accepted `bittyd` Session-Host Contract and ADR 0008 put the daemon
//! above the mechanism crates: `bitty-daemon` may depend on `bitty-term-state`,
//! `bitty-pty`, `bitty-plugin-host`, `bitty-ipc`, and other mechanism crates,
//! and none of them may depend on `bitty-daemon`. Keeping the reverse edge out
//! is what keeps a daemon-shaped global from appearing in a terminal or
//! mechanism crate.
//!
//! These tests read `cargo metadata --no-deps`, so every dependency kind
//! (normal, dev, build, optional, target-specific) is covered without parsing
//! manifest text. They also pin the crate/binary shape the packaging and IPC
//! tasks rely on: one workspace member, one library, exactly one binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Package under test.
const DAEMON: &str = "bitty-daemon";

/// The single binary this crate ships.
const BINARY: &str = "bittyd";

/// Workspace root derived from this crate's manifest dir:
/// `<workspace>/crates/bitty-daemon` -> `<workspace>`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> parent layout")
        .to_path_buf()
}

/// Parsed `cargo metadata --no-deps` for the workspace.
fn metadata() -> Value {
    let root = workspace_root();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .arg("metadata")
        .arg("--no-deps")
        .arg("--locked")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo metadata must run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata must be valid JSON")
}

/// Workspace member ids in metadata order.
fn member_ids(metadata: &Value) -> Vec<&str> {
    metadata["workspace_members"]
        .as_array()
        .expect("workspace_members array")
        .iter()
        .map(|id| id.as_str().expect("member id string"))
        .collect()
}

#[test]
fn no_workspace_crate_depends_on_the_daemon() {
    let metadata = metadata();
    let packages = metadata["packages"].as_array().expect("packages array");
    let mut offenders = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        if name == DAEMON {
            continue;
        }
        for dependency in package["dependencies"]
            .as_array()
            .expect("dependencies array")
        {
            if dependency["name"].as_str() == Some(DAEMON) {
                let kind = dependency["kind"].as_str().unwrap_or("normal");
                offenders.push(format!("{name} -> {DAEMON} ({kind})"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a mechanism crate must never depend on the daemon (ADR 0008 / ADR 0019); \
         reverse dependency edges found:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn daemon_is_a_member_with_exactly_one_binary() {
    let metadata = metadata();
    let members = member_ids(&metadata);
    let packages = metadata["packages"].as_array().expect("packages array");
    assert_eq!(
        packages.len(),
        members.len(),
        "cargo metadata --no-deps must return exactly the workspace members"
    );

    let daemon = packages
        .iter()
        .find(|package| package["name"].as_str() == Some(DAEMON))
        .unwrap_or_else(|| panic!("{DAEMON} must be a workspace member"));
    let daemon_id = daemon["id"].as_str().expect("package id");
    assert!(
        members.contains(&daemon_id),
        "{DAEMON} package id {daemon_id} is missing from workspace_members"
    );

    let targets = daemon["targets"].as_array().expect("targets array");
    let mut binaries = Vec::new();
    let mut libraries = 0;
    for target in targets {
        let kinds = target["kind"].as_array().expect("target kind array");
        if kinds.iter().any(|kind| kind.as_str() == Some("bin")) {
            binaries.push(target["name"].as_str().expect("binary name"));
        }
        if kinds.iter().any(|kind| kind.as_str() == Some("lib")) {
            libraries += 1;
        }
    }
    assert_eq!(
        binaries,
        vec![BINARY],
        "{DAEMON} must ship exactly one binary ({BINARY})"
    );
    assert_eq!(libraries, 1, "{DAEMON} must expose one library target");
}
