//! `bitty upgrade` end-to-end proof (CTX-1719, bitty#2925).
//!
//! Every scenario runs the real binary with a cleared environment, a scratch
//! `XDG_DATA_HOME`/`TMPDIR`, and (where a release is needed) a loopback HTTP
//! server bound to `127.0.0.1:0`. No real network, no host paths, no package
//! manager execution.
//!
//! The self-managed update is proved with one copy of the test binary in the
//! scratch prefix: the served "release asset" is a small shell script that
//! reports `bitty 99.0.0`, so the atomic replace, the `bitty.prev` backup, the
//! post-install `--version` smoke, and the refreshed marker all exercise the
//! production code path. `--rollback` runs from a second install whose backup
//! is a distinguishable script, so "the exact previous bytes" is meaningful.

#![cfg(unix)]

mod upgrade_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use upgrade_support::{Route, Server};

/// Binary under test (compile-time proof the artifact is named `bitty`).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");
/// Hard timeout per invocation (bounded fetches, no hangs).
const TIMEOUT: Duration = Duration::from_secs(90);
/// Version the served release claims (never the running build's version).
const NEXT_VERSION: &str = "99.0.0";
/// Tag the served release is published under.
const NEXT_TAG: &str = "v99.0.0";

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
            "bitty-upgrade-it-{tag}-{}-{sequence}",
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

/// Release triple of the test host (mirrors `upgrade::target_triple`).
fn target_triple() -> &'static str {
    if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        "aarch64-apple-darwin"
    }
}

/// Lowercase hex SHA-256.
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

/// Writes the install marker `install.sh` would have written.
fn write_marker(data_home: &Path, bin_dir: &Path, version: &str, digest: &str) -> PathBuf {
    let marker_dir = data_home.join("bitty");
    std::fs::create_dir_all(&marker_dir).expect("marker dir");
    let marker = marker_dir.join("install.json");
    let prefix = bin_dir.parent().expect("bin dir parent");
    std::fs::write(
        &marker,
        format!(
            "{{\n  \"schema\": 1,\n  \"channel\": \"script\",\n  \"prefix\": \"{}\",\n  \"bin_dir\": \"{}\",\n  \"version\": \"{version}\",\n  \"binary_sha256\": \"{digest}\",\n  \"installed_at\": \"2026-11-06T00:00:00Z\"\n}}\n",
            prefix.display(),
            bin_dir.display(),
        ),
    )
    .expect("write marker");
    marker
}

/// Runs one binary under a cleared environment with a hard timeout.
fn run(binary: &Path, args: &[&str], path: &str, envs: &[(&str, String)]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env_clear()
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn bitty upgrade");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("poll bitty upgrade") {
            Some(_) => return child.wait_with_output().expect("collect output"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bitty upgrade timed out after {TIMEOUT:?} (args={args:?})");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn host_path() -> String {
    std::env::var("PATH").unwrap_or_default()
}

/// No `.bitty-upgrade.*` staging file may survive any outcome.
fn assert_no_staging(bin_dir: &Path) {
    for entry in std::fs::read_dir(bin_dir).expect("read bin dir") {
        let name = entry.expect("dir entry").file_name();
        assert!(
            !name.to_string_lossy().starts_with(".bitty-upgrade."),
            "staging file left behind: {name:?}"
        );
    }
}

/// The install directory and marker shared by the script-channel scenarios.
struct ScriptInstall {
    scratch: Scratch,
    binary: PathBuf,
    bin_dir: PathBuf,
    data_home: PathBuf,
    tmp: PathBuf,
    marker: PathBuf,
    original: Vec<u8>,
    running: &'static str,
}

impl ScriptInstall {
    fn new() -> Self {
        let scratch = Scratch::new("script");
        let bin_dir = scratch.join("prefix/bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let binary = bin_dir.join("bitty");
        std::fs::copy(BITTY_BIN, &binary).expect("copy binary under test");
        let original = std::fs::read(&binary).expect("read original");
        let data_home = scratch.join("data");
        let tmp = scratch.join("tmp");
        std::fs::create_dir_all(&tmp).expect("tmp dir");
        let running = env!("CARGO_PKG_VERSION");
        let marker = write_marker(&data_home, &bin_dir, running, &sha256_hex(&original));
        Self {
            scratch,
            binary,
            bin_dir,
            data_home,
            tmp,
            marker,
            original,
            running,
        }
    }

    fn envs<'a>(&self, extra: &[(&'a str, String)]) -> Vec<(&'a str, String)> {
        let mut envs = vec![
            ("HOME", self.scratch.join("home").display().to_string()),
            ("TMPDIR", self.tmp.display().to_string()),
            ("XDG_DATA_HOME", self.data_home.display().to_string()),
        ];
        envs.extend(extra.iter().map(|(key, value)| (*key, value.clone())));
        envs
    }

    fn run(&self, args: &[&str], base: &str) -> Output {
        run(
            &self.binary,
            args,
            &host_path(),
            &self.envs(&[("BITTY_CDN_BASE", base.to_string())]),
        )
    }

    fn marker_text(&self) -> String {
        std::fs::read_to_string(&self.marker).expect("read marker")
    }

    fn prev(&self) -> PathBuf {
        self.bin_dir.join("bitty.prev")
    }

    fn assert_untouched(&self, marker_before: &str) {
        assert_eq!(
            std::fs::read(&self.binary).expect("binary"),
            self.original,
            "the installed binary must stay byte-identical"
        );
        assert!(!self.prev().exists(), "no backup may be created");
        assert_eq!(
            self.marker_text(),
            marker_before,
            "the marker must stay unchanged"
        );
        assert_no_staging(&self.bin_dir);
    }
}

/// Serves one release: pointer, `SHA256SUMS`, and the asset bytes.
fn release_routes(asset: &str, sums_digest: &str, asset_route: Route) -> Vec<(String, Route)> {
    vec![
        (
            "/install/latest.txt".to_string(),
            Route::ok(format!("{NEXT_TAG}\n")),
        ),
        (
            format!("/releases/{NEXT_TAG}/SHA256SUMS"),
            Route::ok(format!("{sums_digest}  {asset}\n")),
        ),
        (format!("/releases/{NEXT_TAG}/{asset}"), asset_route),
    ]
}

fn pointer_route() -> Vec<(String, Route)> {
    vec![(
        "/install/latest.txt".to_string(),
        Route::ok(format!("{NEXT_TAG}\n")),
    )]
}

/// `--check` compares the published tag with the running build.
#[test]
fn check_reports_current_and_available() {
    let scratch = Scratch::new("check");
    let envs = |base: &str| {
        vec![
            ("HOME", scratch.join("home").display().to_string()),
            ("XDG_DATA_HOME", scratch.join("data").display().to_string()),
            ("BITTY_CDN_BASE", base.to_string()),
        ]
    };

    let ahead = Server::start(pointer_route());
    let output = run(
        Path::new(BITTY_BIN),
        &["upgrade", "--check"],
        &host_path(),
        &envs(&ahead.base()),
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "--check exits 1 when an update exists: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("installed:"), "{stdout}");
    assert!(stdout.contains(NEXT_VERSION), "{stdout}");
    assert!(stdout.contains("update available"), "{stdout}");

    let current = Server::start(vec![(
        "/install/latest.txt".to_string(),
        Route::ok(format!("v{}\n", env!("CARGO_PKG_VERSION"))),
    )]);
    let output = run(
        Path::new(BITTY_BIN),
        &["upgrade", "--check"],
        &host_path(),
        &envs(&current.base()),
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "--check exits 0 when current: {}",
        stderr_text(&output)
    );
    assert!(stdout_text(&output).contains("is current"));
}

/// The script channel: plan, safe mode, refusals, then the atomic replace.
#[test]
fn script_channel_updates_atomically() {
    let install = ScriptInstall::new();
    let asset = format!("bitty-{}", target_triple());
    let new_bytes =
        format!("#!/bin/sh\nprintf 'bitty {NEXT_VERSION} (stable test)\\n'\n").into_bytes();
    let new_digest = sha256_hex(&new_bytes);
    let marker_before = install.marker_text();

    // 1. Read-only plan: nothing is downloaded or written.
    let server = Server::start(pointer_route());
    let output = install.run(&["upgrade", "--dry-run"], &server.base());
    assert_eq!(
        output.status.code(),
        Some(0),
        "--dry-run: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("install source: script"), "{stdout}");
    assert!(
        stdout.contains("dry-run: nothing was downloaded or written"),
        "{stdout}"
    );
    assert!(stdout.contains(&asset), "plan names the asset: {stdout}");
    install.assert_untouched(&marker_before);

    // 2. Safe mode: read-only paths work, writes are refused.
    let output = install.run(&["upgrade", "--safe", "--check"], &server.base());
    assert_eq!(output.status.code(), Some(1), "{}", stderr_text(&output));
    let output = install.run(&["upgrade", "--safe", "--dry-run"], &server.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    let output = install.run(&["upgrade", "--safe"], &server.base());
    assert_eq!(output.status.code(), Some(7), "safe mode refuses writes");
    assert!(stderr_text(&output).contains("safe mode is read-only"));
    install.assert_untouched(&marker_before);

    // 3. Checksum mismatch: the binary is untouched.
    let server = Server::start(release_routes(
        &asset,
        &"0".repeat(64),
        Route::ok(new_bytes.clone()),
    ));
    let output = install.run(&["upgrade"], &server.base());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_text(&output).contains("checksum mismatch"),
        "{}",
        stderr_text(&output)
    );
    install.assert_untouched(&marker_before);

    // 4. Oversized response: refused before any byte is parsed.
    let server = Server::start(release_routes(
        &asset,
        &new_digest,
        Route::oversized(1 << 40),
    ));
    let output = install.run(&["upgrade"], &server.base());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_text(&output).contains("byte bound"),
        "{}",
        stderr_text(&output)
    );
    install.assert_untouched(&marker_before);

    // 5. Redirect to a non-HTTPS URL: refused.
    let server = Server::start(release_routes(
        &asset,
        &new_digest,
        Route::redirect(format!("http://127.0.0.1:9/{asset}")),
    ));
    let output = install.run(&["upgrade"], &server.base());
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr_text(&output).contains("non-HTTPS"),
        "{}",
        stderr_text(&output)
    );
    install.assert_untouched(&marker_before);

    // 6. Success: atomic replace, backup, reported version, refreshed marker.
    let server = Server::start(release_routes(
        &asset,
        &new_digest,
        Route::ok(new_bytes.clone()),
    ));
    let output = install.run(&["upgrade"], &server.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    let stdout = stdout_text(&output);
    assert!(
        stdout.contains(&format!(
            "upgraded bitty {} -> {NEXT_VERSION}",
            install.running
        )),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(&install.binary).expect("binary"),
        new_bytes,
        "the served asset replaced the binary byte for byte"
    );
    assert_eq!(
        std::fs::read(install.prev()).expect("prev"),
        install.original,
        "bitty.prev keeps the exact previous bytes"
    );
    assert_no_staging(&install.bin_dir);
    let marker = install.marker_text();
    assert_eq!(
        json_field(&marker, "version").as_deref(),
        Some(NEXT_VERSION)
    );
    assert_eq!(
        json_field(&marker, "binary_sha256").as_deref(),
        Some(new_digest.as_str())
    );
    assert_eq!(json_field(&marker, "channel").as_deref(), Some("script"));
    assert_eq!(
        json_field(&marker, "bin_dir").as_deref(),
        Some(install.bin_dir.display().to_string().as_str())
    );
}

/// `--rollback` restores the exact previous bytes after verifying the backup.
#[test]
fn rollback_restores_the_previous_bytes() {
    let install = ScriptInstall::new();
    // The backup is a distinguishable executable that reports an older
    // version, so "the exact previous bytes" is provable.
    let backup_bytes = b"#!/bin/sh\nprintf 'bitty 0.0.32 (stable test)\\n'\n";
    let backup = install.prev();
    std::fs::write(&backup, backup_bytes).expect("write backup");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o755))
            .expect("chmod backup");
    }
    let server = Server::start(pointer_route());

    // Dry run: the plan is printed, nothing changes.
    let output = install.run(&["upgrade", "--rollback", "--dry-run"], &server.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert!(stdout_text(&output).contains("dry-run: nothing was written"));
    assert!(backup.exists(), "--dry-run keeps the backup");
    assert_eq!(
        std::fs::read(&install.binary).expect("binary"),
        install.original
    );

    let output = install.run(&["upgrade", "--rollback"], &server.base());
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert!(
        stdout_text(&output).contains("rolled back to bitty 0.0.32"),
        "{}",
        stdout_text(&output)
    );
    assert_eq!(
        std::fs::read(&install.binary).expect("binary"),
        backup_bytes,
        "rollback restores the exact previous bytes"
    );
    assert!(!backup.exists(), "the backup is consumed");
    let marker = install.marker_text();
    assert_eq!(
        json_field(&marker, "version").as_deref(),
        Some("0.0.32"),
        "the marker follows the restored binary"
    );
    assert_eq!(
        json_field(&marker, "binary_sha256").as_deref(),
        Some(sha256_hex(backup_bytes).as_str())
    );

    // A second rollback has nothing to restore. The installed file is now the
    // restored script, so put a real binary back to run the command again.
    std::fs::copy(BITTY_BIN, &install.binary).expect("restore a runnable binary");
    let output = install.run(&["upgrade", "--rollback"], &server.base());
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr_text(&output).contains("nothing to roll back"));
}

/// System-owned installs print the command, execute nothing, exit 7.
#[test]
fn system_owned_channel_prints_command_and_exits_permission() {
    let scratch = Scratch::new("apt");
    let stubs = scratch.join("stubs");
    std::fs::create_dir_all(&stubs).expect("stub dir");
    let log = scratch.join("invocations.log");
    let stub = stubs.join("dpkg-query");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .expect("write stub");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }
    let path = stubs.display().to_string();
    let envs = vec![
        ("HOME", scratch.join("home").display().to_string()),
        ("XDG_DATA_HOME", scratch.join("data").display().to_string()),
        ("BITTY_CDN_BASE", "https://cdn.invalid".to_string()),
    ];

    // The version pointer is never fetched for a system-owned install: the
    // detection result is enough to refuse before any network access.
    let output = run(Path::new(BITTY_BIN), &["upgrade"], &path, &envs);
    assert_eq!(
        output.status.code(),
        Some(7),
        "system-owned installs exit with the permission code: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("install source: apt"), "{stdout}");
    assert!(
        stdout.contains("apt-get install --only-upgrade bitty"),
        "the exact command is printed: {stdout}"
    );
    assert!(stderr_text(&output).contains("system-owned"));

    // --yes never changes that: nothing is executed.
    let output = run(Path::new(BITTY_BIN), &["upgrade", "--yes"], &path, &envs);
    assert_eq!(output.status.code(), Some(7));

    // A plan is not an attempt: --dry-run prints the command and exits 0.
    let output = run(
        Path::new(BITTY_BIN),
        &["upgrade", "--dry-run"],
        &path,
        &envs,
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr_text(&output));
    assert!(stdout_text(&output).contains("dry-run: nothing was executed"));

    let invocations = std::fs::read_to_string(&log).expect("stub log");
    let lines: Vec<&str> = invocations.lines().collect();
    assert_eq!(lines.len(), 3, "only ownership queries ran: {lines:?}");
    for line in lines {
        let mut fields = line.split_whitespace();
        assert_eq!(fields.next(), Some("-S"), "ownership query only: {line}");
        assert!(
            fields.next().is_some(),
            "the query names the binary: {line}"
        );
        assert!(fields.next().is_none(), "one query, no extra args: {line}");
        for tool in ["apt-get", "apt", "dnf", "pacman", "brew", "scoop"] {
            assert!(
                !line.contains(tool),
                "no package-manager command ran ({tool}): {line}"
            );
        }
    }
}

/// Unknown install source: manual guidance and the docs link, exit 1.
#[test]
fn unknown_channel_refuses_with_manual_guidance() {
    let scratch = Scratch::new("unknown");
    let empty = scratch.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    let envs = vec![
        ("HOME", scratch.join("home").display().to_string()),
        ("XDG_DATA_HOME", scratch.join("data").display().to_string()),
    ];
    let output = run(
        Path::new(BITTY_BIN),
        &["upgrade"],
        &empty.display().to_string(),
        &envs,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("cannot detect how bitty was installed"),
        "{stderr}"
    );
    assert!(stderr.contains("brew upgrade bitty"), "{stderr}");
    assert!(stderr.contains("packaging/upgrade.md"), "{stderr}");
    assert!(stdout_text(&output).contains("install source: unknown"));
}

/// Usage errors fail closed before any fetch.
#[test]
fn usage_errors_fail_closed() {
    let scratch = Scratch::new("usage");
    let empty = scratch.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    let envs = vec![
        ("HOME", scratch.join("home").display().to_string()),
        ("XDG_DATA_HOME", scratch.join("data").display().to_string()),
        ("BITTY_CDN_BASE", "https://cdn.invalid".to_string()),
    ];
    let path = empty.display().to_string();

    for args in [
        vec!["upgrade", "--check", "--rollback"],
        vec!["upgrade", "--rollback", "--yes"],
        vec!["upgrade", "now"],
    ] {
        let output = run(Path::new(BITTY_BIN), &args, &path, &envs);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} is a usage error: {}",
            stderr_text(&output)
        );
        assert!(stderr_text(&output).contains("usage: bitty upgrade"));
    }
}

/// `--help` (and `bitty help upgrade`) are local, stable, and identical.
#[test]
fn help_is_local_and_stable() {
    let scratch = Scratch::new("help");
    let empty = scratch.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    let path = empty.display().to_string();
    let envs = vec![
        ("HOME", scratch.join("home").display().to_string()),
        ("XDG_DATA_HOME", scratch.join("data").display().to_string()),
    ];
    let direct = run(Path::new(BITTY_BIN), &["upgrade", "--help"], &path, &envs);
    assert_eq!(direct.status.code(), Some(0), "{}", stderr_text(&direct));
    let text = stdout_text(&direct);
    for needle in [
        "usage: bitty upgrade",
        "--check",
        "--dry-run",
        "--rollback",
        "packaging/upgrade.md",
    ] {
        assert!(text.contains(needle), "help names {needle}: {text}");
    }

    let alias = run(Path::new(BITTY_BIN), &["help", "upgrade"], &path, &envs);
    assert_eq!(alias.status.code(), Some(0));
    assert_eq!(
        stdout_text(&alias),
        text,
        "help alias prints the same bytes"
    );
}
