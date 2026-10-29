//! bitty#2606 / CTX-1616: `bittyd` binary seam behavior.
//!
//! The binary must answer `--version`, must report the service status (no
//! endpoint bound) with no arguments, and must create no runtime file while
//! doing so.
//! The no-argument tests run with a throwaway `XDG_RUNTIME_DIR` so a future
//! accidental socket bind or lock file is caught here, not on a user host; one
//! variant also sets `BITTYD_ENDPOINT` to prove the seam ignores the override
//! instead of creating the requested path.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// Built by Cargo for this integration test.
const BITTYD: &str = env!("CARGO_BIN_EXE_bittyd");

/// Distinguishes temp dirs of parallel tests in one process.
static NEXT_TEMP_DIR: AtomicU32 = AtomicU32::new(0);

/// Minimal temporary directory guard (no external dev-dependency).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let suffix = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bitty-daemon-{label}-{}-{suffix}",
            std::process::id()
        ));
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("clear stale temp dir");
        }
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Every entry under `root`, recursively.
fn entries(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read temp dir") {
            let path = entry.expect("temp entry").path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}

#[test]
fn version_reports_the_crate_and_service_versions() {
    let output = Command::new(BITTYD)
        .arg("--version")
        .output()
        .expect("run bittyd --version");
    assert!(output.status.success(), "bittyd --version must exit 0");
    assert!(
        output.stderr.is_empty(),
        "bittyd --version wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "crate version missing from: {stdout}"
    );
    assert!(
        stdout.contains("service API 1.2"),
        "contract service version missing from: {stdout}"
    );
}

#[test]
fn no_arguments_reports_the_seam_and_creates_no_endpoint() {
    let runtime_dir = TempDir::new("status");
    let output = Command::new(BITTYD)
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env_remove("BITTYD_ENDPOINT")
        .output()
        .expect("run bittyd with no arguments");
    assert!(
        output.status.success(),
        "bittyd with no arguments must exit 0"
    );
    assert!(
        output.stderr.is_empty(),
        "bittyd seam status wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("session-host seam"), "stdout: {stdout}");
    assert!(
        stdout.contains("ipc service: wired (bitty-ipc v1"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("endpoint: none bound"), "stdout: {stdout}");

    let socket = runtime_dir.path().join("bitty").join("bittyd.sock");
    assert!(
        !socket.exists(),
        "the seam must not create a socket file: {}",
        socket.display()
    );
    let created = entries(runtime_dir.path());
    assert!(
        created.is_empty(),
        "the seam must not create runtime files, found: {created:?}"
    );
}

#[test]
fn endpoint_override_creates_nothing() {
    let runtime_dir = TempDir::new("endpoint");
    let endpoint = runtime_dir
        .path()
        .join("explicit")
        .join("bittyd-requested.sock");
    let output = Command::new(BITTYD)
        .env("XDG_RUNTIME_DIR", runtime_dir.path())
        .env("BITTYD_ENDPOINT", &endpoint)
        .output()
        .expect("run bittyd with an endpoint override");
    assert!(
        output.status.success(),
        "bittyd with an endpoint override must exit 0"
    );
    assert!(
        output.stderr.is_empty(),
        "bittyd seam status wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("session-host seam"), "stdout: {stdout}");
    assert!(stdout.contains("endpoint: none bound"), "stdout: {stdout}");
    assert!(
        !endpoint.exists(),
        "the seam must not create the requested endpoint: {}",
        endpoint.display()
    );
    let created = entries(runtime_dir.path());
    assert!(
        created.is_empty(),
        "the seam must not create runtime files, found: {created:?}"
    );
}

#[test]
fn unknown_argument_fails_with_usage() {
    let output = Command::new(BITTYD)
        .arg("--listen")
        .output()
        .expect("run bittyd with an unknown argument");
    assert_eq!(
        output.status.code(),
        Some(2),
        "unknown arguments must exit 2"
    );
    assert!(
        output.stdout.is_empty(),
        "usage errors write to stderr, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown argument '--listen'"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("usage: bittyd"), "stderr: {stderr}");
}
