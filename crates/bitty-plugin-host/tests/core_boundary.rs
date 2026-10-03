//! Core package-manager boundary tests (W-101 / CTX-0927, `bitty#1616`).
//!
//! These are the negative and invariant tests for the accepted
//! `package-manager-boundary.md` contract:
//!
//! 1. the retained Core crates (`bitty-package`, `bitty-plugin-host`) contain
//!    no network egress API and depend on no network implementation crate —
//!    Core never fetches (`DIR-016`/`DIR-017`);
//! 2. the read-only installed-generation validation and the install-time
//!    verification seam fail closed on tampered integrity and on capability
//!    escalation (`P0-AC-012`, `P0-AC-028`, `P0-AC-030`).
//!
//! Every test is offline and deterministic; no test opens a socket.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use bitty_package::{
    InstalledGenerationInputs, PackageId, PackageIdentity, sha256_hex,
    validate_installed_generation,
};
use bitty_plugin_host::install::{InstallInputs, verify_install};

// ── network-egress source scan ───────────────────────────────────────────

/// API tokens that would indicate a network implementation or egress path.
///
/// The closed capability *name* `network.connect` is intentionally absent:
/// declaring the capability is not an egress path, and a package that
/// declares it still runs in the Core process with no socket linked.
const NETWORK_EGRESS_TOKENS: &[&str] = &[
    "std::net",
    "core::net",
    "std::os::unix::net",
    "std::os::windows::net",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "reqwest::",
    "hyper::",
    "ureq::",
    "isahc::",
    "curl::",
    "awc::",
    "surf::",
    "attohttpc::",
    "tungstenite::",
    "quinn::",
    "rustls::",
    "native_tls::",
    "openssl::",
    "tokio::net",
    "async_std::net",
    "smol::net",
    "socket2::",
    "hickory_resolver",
    "trust_dns_resolver",
];

/// First network-egress token found in `source`, if any.
fn first_network_egress_token(source: &str) -> Option<&'static str> {
    NETWORK_EGRESS_TOKENS
        .iter()
        .copied()
        .find(|token| source.contains(token))
}

#[test]
fn egress_scan_detects_a_seeded_network_call() {
    let hostile = "let s = std::net::TcpStream::connect(\"example.com:80\")?;";
    assert_eq!(
        first_network_egress_token(hostile),
        Some("std::net"),
        "scan must not be vacuously clean"
    );
    let benign = "let c = CapabilityId::new(\"network.connect:example\")?;";
    assert_eq!(first_network_egress_token(benign), None);
}

/// Recursively collect `.rs` files under `root`.
fn collect_rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(root).expect("read source dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn workspace_root() -> PathBuf {
    // <root>/crates/bitty-plugin-host/../..
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

#[test]
fn retained_core_crates_have_no_network_egress_api() {
    let crate_manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        crate_manifest_dir.join("src"),
        crate_manifest_dir
            .join("..")
            .join("bitty-package")
            .join("src"),
        crate_manifest_dir
            .join("..")
            .join("bitty-runtime")
            .join("src")
            .join("plugin_runtime"),
    ];
    let mut violations = Vec::new();
    for root in roots {
        let mut sources = Vec::new();
        collect_rust_sources(&root, &mut sources);
        assert!(
            !sources.is_empty(),
            "no sources found under {}",
            root.display()
        );
        for source in sources {
            let text = std::fs::read_to_string(&source).expect("read source");
            if let Some(token) = first_network_egress_token(&text) {
                violations.push(format!("{} contains '{token}'", source.display()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Core package/runtime-load path must have no network egress:\n{}",
        violations.join("\n")
    );
}
// ── dependency-graph closure ─────────────────────────────────────────────

/// Network implementation crates Core must never link.
const NETWORK_IMPLEMENTATION_CRATES: &[&str] = &[
    "reqwest",
    "hyper",
    "h2",
    "ureq",
    "isahc",
    "curl",
    "curl-sys",
    "awc",
    "surf",
    "attohttpc",
    "tungstenite",
    "tokio-tungstenite",
    "async-tungstenite",
    "quinn",
    "rustls",
    "native-tls",
    "openssl",
    "openssl-sys",
    "tokio",
    "async-std",
    "smol",
    "mio",
    "socket2",
    "hickory-resolver",
    "trust-dns-resolver",
];

#[test]
fn core_package_crates_link_no_network_implementation_crate() {
    // `cargo tree --edges normal` is the authoritative *linked* dependency
    // graph: unlike a raw Cargo.lock walk it does not follow dev- or
    // build-only edges (which is how a wasm-only `tokio` node appears in the
    // lock without being linked by these crates). `--offline --locked` keeps
    // the check hermetic and reproducible.
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Populate the complete dependency closure first: running this test alone
    // (`cargo test -p bitty-plugin-host`) does not fetch `bitty-runtime` or
    // its dependencies, and the `--offline` tree below would then fail during
    // resolution on a clean cache. Fetch failures fail the test rather than
    // skipping, so the boundary check is never silently dropped.
    let fetch = std::process::Command::new(&cargo)
        .args(["fetch", "--locked"])
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|error| panic!("run `{cargo} fetch`: {error}"));
    assert!(
        fetch.status.success(),
        "`{cargo} fetch --locked` failed: {}",
        String::from_utf8_lossy(&fetch.stderr)
    );
    let output = std::process::Command::new(&cargo)
        .args([
            "tree",
            "-p",
            "bitty-package",
            "-p",
            "bitty-plugin-host",
            "-p",
            "bitty-runtime",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--offline",
            "--locked",
        ])
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|error| panic!("run `{cargo} tree`: {error}"));
    assert!(
        output.status.success(),
        "`{cargo} tree` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        tree.contains("bitty-package"),
        "dependency tree for the Core package/runtime-load closure looks empty"
    );
    let linked: Vec<&str> = NETWORK_IMPLEMENTATION_CRATES
        .iter()
        .copied()
        .filter(|candidate| {
            tree.lines()
                .filter_map(|line| line.split_whitespace().next())
                .any(|package| package == *candidate)
        })
        .collect();
    assert!(
        linked.is_empty(),
        "Core package/runtime-load dependency closure must link no network implementation crate; found: {linked:?}"
    );
}

// ── fail-closed negative tests ───────────────────────────────────────────

/// A closed install fixture mirroring `bitty-plugin-host::install` tests.
struct InstallFixture {
    artifact: Vec<u8>,
    artifact_digest: String,
    manifest: bitty_package::PackageManifest,
    manifest_digest: String,
}

impl InstallFixture {
    fn new(capability: Option<&str>) -> Self {
        let capabilities = capability
            .map(|raw| vec![bitty_package::CapabilityId::new(raw).expect("capability")])
            .unwrap_or_default();
        let manifest = bitty_package::PackageManifest {
            identity: PackageIdentity {
                id: PackageId::new("xuepoo.boundary").expect("id"),
                name: "Boundary".to_string(),
                version: "0.1.0".to_string(),
                description: "boundary fixture".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: bitty_package::Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities,
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        };
        let artifact_digest = sha256_hex(b"boundary artifact bytes");
        let manifest_digest = manifest.canonical_digest();
        Self {
            artifact: b"boundary artifact bytes".to_vec(),
            artifact_digest,
            manifest,
            manifest_digest,
        }
    }

    fn inputs<'a>(
        &'a self,
        artifact: &'a [u8],
        granted: &'a [String],
        requested: &'a [String],
        approval: bool,
    ) -> InstallInputs<'a> {
        InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &self.artifact_digest,
            manifest: &self.manifest,
            expected_manifest_digest: &self.manifest_digest,
            granted_capabilities: granted,
            requested_capabilities: requested,
            capability_approval: approval,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.boundary",
            trust_mode: bitty_package::TrustMode::PinningOnly,
            candidate_identity: None,
            trust_store: None,
            signature: None,
            key_store: None,
            environment: None,
        }
    }
}

#[test]
fn install_verification_rejects_tampered_artifact() {
    let fixture = InstallFixture::new(None);
    let tampered = b"tampered artifact bytes";
    let inputs = fixture.inputs(tampered, &[], &[], false);
    assert!(
        verify_install(&inputs).is_err(),
        "a tampered artifact must fail closed before staging"
    );
}

#[test]
fn install_verification_blocks_capability_increase_without_approval() {
    let fixture = InstallFixture::new(Some("terminal.semantic-read"));
    let granted: Vec<String> = Vec::new();
    let requested = vec!["terminal.semantic-read".to_string()];
    let inputs = fixture.inputs(&fixture.artifact, &granted, &requested, false);
    assert!(
        verify_install(&inputs).is_err(),
        "a capability increase without approval must block (P0-AC-030)"
    );
}

#[test]
fn startup_reverification_rejects_undeclared_capability_grant() {
    // The plugin declares nothing; a recorded grant of a closed-set capability
    // that the manifest does not declare is store tampering and must fail the
    // startup re-derivation (deny-by-default, no escalation).
    let fixture = InstallFixture::new(None);
    let granted = vec!["terminal.semantic-read".to_string()];
    let inputs = InstalledGenerationInputs {
        manifest: &fixture.manifest,
        artifact_bytes: &fixture.artifact,
        expected_artifact_digest: &fixture.artifact_digest,
        expected_manifest_digest: &fixture.manifest_digest,
        granted_capabilities: &granted,
    };
    assert!(
        validate_installed_generation(&inputs).is_err(),
        "an undeclared recorded grant must fail closed at read-only validation"
    );
}

#[test]
fn read_only_validation_rejects_tampered_artifact() {
    let fixture = InstallFixture::new(Some("terminal.semantic-read"));
    let granted = vec!["terminal.semantic-read".to_string()];
    let inputs = InstalledGenerationInputs {
        manifest: &fixture.manifest,
        artifact_bytes: b"tampered",
        expected_artifact_digest: &fixture.artifact_digest,
        expected_manifest_digest: &fixture.manifest_digest,
        granted_capabilities: &granted,
    };
    assert!(validate_installed_generation(&inputs).is_err());
}
