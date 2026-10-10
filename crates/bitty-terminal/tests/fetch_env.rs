//! Fetch-environment hostile proof (issue #1907, unix only).
//!
//! A fake `curl` on `PATH` serves a genuine locally packed component release
//! while dumping every fetch child's environment to a file. The install runs
//! under poisoned secret-shaped variables plus canary/proxy sentinels, so
//! one test proves both halves of the posture: the fetch still works (exit
//! 0, staged payload) and no `*_TOKEN`/`*_KEY`/`*_SECRET` variable reaches a
//! fetch child (dump audit) while proxy/canary inheritance stays intact.
//!
//! Unix-gated: the fake `curl` is a `sh` script (Windows needs a separate
//! helper and stays on the fail-closed unit path). Skips (does not panic)
//! when `tar` is absent or the host has no seed triple.

// Whole-file unix gate: on Windows this target compiles to empty (no
// dead-code warnings under `-D warnings` in the windows-gnu check).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Binary under test.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Fresh isolated home for one test.
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitty-fetch-env-{tag}-{}-{}",
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn user_root(home: &Path) -> PathBuf {
    home.join("bitty").join("components")
}

/// Host seed triple for this test binary (`None` on unmapped OS/arch).
#[cfg(unix)]
fn host_seed_triple() -> Option<&'static str> {
    if cfg!(target_env = "musl") {
        return match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
            _ => None,
        };
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

#[cfg(unix)]
#[test]
fn fetch_env_hostile_secrets_scrubbed_and_fetch_still_works() {
    if Command::new("tar").arg("--version").output().is_err() {
        eprintln!("SKIP: tar not found (packs the genuine fixture release)");
        return;
    }
    let Some(target) = host_seed_triple() else {
        eprintln!("SKIP: host has no seed triple");
        return;
    };
    let home = scratch_dir("hostile");
    let system = home.join("system-components");
    std::fs::create_dir_all(&system).expect("system dir");

    // Genuine release layout: descriptor plus executable at the archive
    // root, packed exactly like `make-component-dist.sh`.
    let core = bitty_runtime::component::PROTOCOL_VERSION;
    let exe = b"fetch-env-dist-bytes";
    let exe_digest = bitty_package::integrity::sha256_hex(exe);
    let src = home.join("src");
    std::fs::create_dir_all(&src).expect("src");
    std::fs::write(src.join("bitty-net"), exe).expect("exe");
    std::fs::write(
        src.join("bitty-component.toml"),
        format!(
            "[component]\nname = \"net\"\nversion = \"0.0.23\"\nprotocol = [{core}, {core}]\nexecutable = \"bitty-net\"\nsha256 = \"{exe_digest}\"\n"
        ),
    )
    .expect("descriptor");
    let tarball = home.join("payload.tar.gz");
    let pack = Command::new("tar")
        .args([
            "-czf",
            &tarball.display().to_string(),
            "-C",
            &src.display().to_string(),
            "bitty-component.toml",
            "bitty-net",
        ])
        .output()
        .expect("pack");
    assert!(pack.status.success(), "pack failed: {pack:?}");
    let tarball_file = format!("{target}.tar.gz");
    let manifest = home.join("SHA256SUMS");
    std::fs::write(
        &manifest,
        format!(
            "{}  {tarball_file}\n",
            bitty_package::integrity::sha256_hex(&std::fs::read(&tarball).expect("tarball"))
        ),
    )
    .expect("manifest");

    // Fake `curl`: reports a new version for the preflight, dumps each
    // fetch child's environment, then serves the fixture bytes by URL.
    let fakebin = home.join("fakebin");
    std::fs::create_dir_all(&fakebin).expect("fakebin");
    let dump = home.join("child-env.dump");
    std::fs::write(
        fakebin.join("curl"),
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then\n\
         \x20 echo 'curl 8.22.0 (x86_64-pc-linux-gnu) libcurl/8.22.0 OpenSSL/3.6.4'\n\
         \x20 exit 0\n\
         fi\n\
         dest=''\n\
         prev=''\n\
         url=''\n\
         for arg in \"$@\"; do\n\
         \x20 if [ \"$prev\" = '--output' ]; then dest=\"$arg\"; fi\n\
         \x20 prev=\"$arg\"\n\
         \x20 url=\"$arg\"\n\
         done\n\
         { echo \"--- fetch $url\"; env; echo '--- end'; } >> \"$FAKE_CURL_ENV_DUMP\"\n\
         case \"$url\" in\n\
         \x20 */SHA256SUMS) cat \"$FAKE_CURL_MANIFEST\" > \"$dest\" ;;\n\
         \x20 *) cat \"$FAKE_CURL_TARBALL\" > \"$dest\" ;;\n\
         esac\n",
    )
    .expect("fake curl");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(fakebin.join("curl"), std::fs::Permissions::from_mode(0o755))
            .expect("mode");
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = if old_path.is_empty() {
        fakebin.display().to_string()
    } else {
        format!("{}:{old_path}", fakebin.display())
    };

    // Poisoned secrets plus canary/proxy sentinels, all via the child
    // environment (no process-env mutation, no `unsafe`).
    let mut command = Command::new(BITTY_BIN);
    command
        .args([
            "component",
            "install",
            "net",
            "--version",
            "0.0.23",
            "--yes",
        ])
        .env("XDG_CONFIG_HOME", &home)
        .env("XDG_DATA_HOME", &home)
        .env("HOME", &home)
        .env("BITTY_SYSTEM_COMPONENTS_DIR", &system)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_COMPONENTS_DIR")
        .env_remove("BITTY_PLUGIN_DIR")
        .env("PATH", &path)
        .env("FAKE_CURL_MANIFEST", &manifest)
        .env("FAKE_CURL_TARBALL", &tarball)
        .env("FAKE_CURL_ENV_DUMP", &dump)
        .env("BITTY_CTX1112_GITHUB_TOKEN", "poison-token")
        .env("BITTY_CTX1112_AWS_SECRET_ACCESS_KEY", "poison-secret")
        .env("BITTY_CTX1112_API_KEY", "poison-key")
        .env("BITTY_CTX1112_CANARY", "canary")
        .env("https_proxy", "http://sentinel-proxy.invalid:8080")
        .env("no_proxy", "sentinel-noproxy")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let output = command.output().expect("run bitty");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("installed"), "{}", stdout(&output));
    assert!(
        user_root(&home)
            .join("net/0.0.23/bitty-component.toml")
            .is_file(),
        "descriptor must be staged"
    );
    assert_eq!(
        std::fs::read_to_string(user_root(&home).join("net/current")).expect("current"),
        "0.0.23\n"
    );

    // Dump audit: both fetches ran, no poisoned var reached either child,
    // and proxy/canary inheritance stayed intact.
    let dump_text = std::fs::read_to_string(&dump).expect("dump");
    assert!(
        dump_text.contains("--- fetch https://cdn.bitty.run/"),
        "fake curl must have served fetches: {dump_text}"
    );
    for poisoned in [
        "BITTY_CTX1112_GITHUB_TOKEN",
        "BITTY_CTX1112_AWS_SECRET_ACCESS_KEY",
        "BITTY_CTX1112_API_KEY",
    ] {
        assert!(
            !dump_text.lines().any(|line| line.starts_with(poisoned)),
            "child env leaks {poisoned}: {dump_text}"
        );
    }
    assert!(
        dump_text
            .lines()
            .any(|line| line == "BITTY_CTX1112_CANARY=canary"),
        "child env must inherit non-secret vars: {dump_text}"
    );
    assert!(
        dump_text
            .lines()
            .any(|line| line == "https_proxy=http://sentinel-proxy.invalid:8080"),
        "child env must inherit the proxy env: {dump_text}"
    );
    assert!(
        dump_text
            .lines()
            .any(|line| line == "no_proxy=sentinel-noproxy"),
        "child env must inherit no_proxy: {dump_text}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
