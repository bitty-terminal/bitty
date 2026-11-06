//! `install.sh` install-marker proof (CTX-1719, bitty#2925).
//!
//! The installer is run for real in a scratch prefix against a `file://` CDN
//! base (curl reads local files), so the test proves the marker the production
//! script writes without any network access. The artifact is a small shell
//! script standing in for the macOS bare-binary bundle, which keeps the test
//! free of `tar`/`zstd`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

/// Hard timeout for one installer run.
const TIMEOUT: Duration = Duration::from_secs(120);
/// Version the scratch release publishes.
const VERSION: &str = "0.0.99";

/// Self-removing scratch directory.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bitty-install-marker-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for index in 0..digest.len() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{:02x}", digest[index]);
    }
    hex
}

/// Extracts one flat string field from the marker JSON.
fn json_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = text.find(&needle)? + needle.len();
    let rest = text[start..].trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Runs `install.sh` with a cleared environment.
fn run_install(
    script: &Path,
    args: &[&str],
    cdn_base: &str,
    data_home: &Path,
    home: &Path,
    tmp: &Path,
) -> Output {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut command = Command::new("bash");
    command
        .arg(script)
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", home)
        .env("TMPDIR", tmp)
        .env("XDG_DATA_HOME", data_home)
        .env("BITTY_CDN_BASE", cdn_base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn install.sh");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("poll install.sh") {
            Some(_) => return child.wait_with_output().expect("collect installer output"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("install.sh timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// `install.sh` writes the marker with channel/prefix/bin_dir/version/hash/time.
#[test]
fn install_sh_writes_the_install_marker() {
    let scratch = Scratch::new("marker");
    let cdn = scratch.join("cdn");
    let artifact_name = "bitty-aarch64-apple-darwin";
    let release_dir = cdn.join(format!("releases/v{VERSION}"));
    std::fs::create_dir_all(&release_dir).expect("release dir");
    let artifact = format!("#!/bin/sh\nprintf 'bitty {VERSION} (stable test)\\n'\n").into_bytes();
    std::fs::write(release_dir.join(artifact_name), &artifact).expect("write artifact");
    std::fs::write(
        release_dir.join(format!("{artifact_name}.sha256")),
        format!("{}  {artifact_name}\n", sha256_hex(&artifact)),
    )
    .expect("write sidecar");

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
    assert!(
        script.is_file(),
        "install.sh must exist at {}",
        script.display()
    );
    let prefix = scratch.join("prefix");
    let data_home = scratch.join("data");
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let base = format!("file://{}", cdn.display());

    let output = run_install(
        &script,
        &[
            "--version",
            VERSION,
            "--target",
            "aarch64-apple-darwin",
            "--prefix",
            &prefix.display().to_string(),
        ],
        &base,
        &data_home,
        &home,
        &tmp,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "install.sh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = prefix.join("bin/bitty");
    assert_eq!(
        std::fs::read(&installed).expect("installed binary"),
        artifact,
        "the verified artifact is installed"
    );

    let marker_path = data_home.join("bitty/install.json");
    let marker = std::fs::read_to_string(&marker_path).expect("read marker");
    assert!(marker.contains("\"schema\": 1"), "schema field: {marker}");
    assert_eq!(json_field(&marker, "channel").as_deref(), Some("script"));
    assert_eq!(
        json_field(&marker, "prefix").as_deref(),
        Some(prefix.display().to_string().as_str())
    );
    assert_eq!(
        json_field(&marker, "bin_dir").as_deref(),
        Some(prefix.join("bin").display().to_string().as_str())
    );
    assert_eq!(json_field(&marker, "version").as_deref(), Some(VERSION));
    assert_eq!(
        json_field(&marker, "binary_sha256").as_deref(),
        Some(sha256_hex(&artifact).as_str())
    );
    let installed_at = json_field(&marker, "installed_at").expect("installed_at");
    assert!(
        installed_at.len() == 20
            && installed_at.ends_with('Z')
            && installed_at.as_bytes()[4] == b'-'
            && installed_at.as_bytes()[10] == b'T',
        "installed_at is a UTC timestamp: {installed_at}"
    );
}

/// `--dry-run` writes no marker and creates no prefix.
#[test]
fn install_sh_dry_run_writes_no_marker() {
    let scratch = Scratch::new("dry");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh");
    let prefix = scratch.join("prefix");
    let data_home = scratch.join("data");
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    std::fs::create_dir_all(&tmp).expect("tmp dir");

    let output = run_install(
        &script,
        &[
            "--version",
            VERSION,
            "--target",
            "aarch64-apple-darwin",
            "--prefix",
            &prefix.display().to_string(),
            "--dry-run",
        ],
        "file:///nonexistent",
        &data_home,
        &home,
        &tmp,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !data_home.join("bitty/install.json").exists(),
        "--dry-run writes no marker"
    );
    assert!(!prefix.exists(), "--dry-run creates no prefix");
}
