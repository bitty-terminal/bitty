//! Build-time metadata for `bitty`: version channel/commit plus, on Windows
//! targets, the executable's icon resource.
//!
//! # Version metadata
//!
//! Emits `BITTY_CHANNEL` and `BITTY_COMMIT` as compile-time env vars consumed
//! by `crate::version` via `option_env!`. Total and infallible by design: every
//! probe degrades to a static fallback so packaging builds without git, without
//! a network, or on Windows still compile.
//!
//! - `BITTY_CHANNEL`: honors a caller-provided env value first (release
//!   engineering sets `stable`/`beta`/etc.); otherwise `stable` for release
//!   profiles and `dev` for everything else.
//! - `BITTY_COMMIT`: honors a caller-provided env value first; otherwise the
//!   short `git rev-parse HEAD` of the build tree; otherwise `unknown`.
//!
//! # Windows icon (bitty#2929)
//!
//! The Windows binary links as a GUI-subsystem executable, and Windows takes
//! the window/shortcut icon from the executable's resources, so `bitty.rc`
//! (an `ICON` statement over `packaging/icons/bitty.ico`) is compiled with
//! `embed-resource` whenever the *target* OS is Windows. `embed-resource`
//! picks the platform resource compiler (`llvm-rc` for MSVC targets, `windres`
//! for GNU ones) and fails the build loudly, so a broken resource can never
//! ship unnoticed.
//!
//! The dependency lives in the host-gated
//! `[target.'cfg(windows)'.build-dependencies]` table because Cargo resolves
//! build-dependency target tables against the *host*, not the target. A
//! Windows host therefore always embeds the icon (every release artifact is
//! built on `windows-latest`), while a Linux host cross-compiling to Windows
//! warns instead of silently producing an icon-less binary.

#[cfg(windows)]
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=BITTY_CHANNEL");
    println!("cargo:rerun-if-env-changed=BITTY_COMMIT");

    let profile = std::env::var("PROFILE").unwrap_or_default();
    let channel = std::env::var("BITTY_CHANNEL").unwrap_or_else(|_| {
        if profile == "release" {
            String::from("stable")
        } else {
            String::from("dev")
        }
    });
    println!("cargo:rustc-env=BITTY_CHANNEL={channel}");

    let commit = std::env::var("BITTY_COMMIT").unwrap_or_else(|_| git_short_head());
    println!("cargo:rustc-env=BITTY_COMMIT={commit}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
}

/// Compiles `bitty.rc` into the Windows executable (bitty#2929).
#[cfg(windows)]
fn embed_windows_icon() {
    let rc = Path::new(env!("CARGO_MANIFEST_DIR")).join("bitty.rc");
    println!("cargo:rerun-if-changed={}", rc.display());
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons/bitty.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    if !icon.is_file() {
        panic!(
            "Windows icon missing at {}: generate it from packaging/icons/hicolor/256x256/apps/bitty.png",
            icon.display()
        );
    }
    match embed_resource::compile(&rc, embed_resource::NONE).manifest_optional() {
        Ok(()) => {}
        Err(err) => panic!("failed to embed the Windows icon resource: {err}"),
    }
}

/// Cross-compiling a Windows target on a non-Windows host (bitty#2929).
///
/// Cargo resolves build-dependency target tables against the host, so the
/// resource compiler is only available on a Windows host. Warn instead of
/// producing an icon-less executable silently.
#[cfg(not(windows))]
fn embed_windows_icon() {
    println!(
        "cargo:warning=Windows target on a non-Windows host: the icon resource is not embedded; build on Windows for a shipped artifact"
    );
}

/// Best-effort short commit hash of the build tree; `unknown` when git is
/// absent, fails, or yields an empty/unshaped value.
fn git_short_head() -> String {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let hash = raw.trim();
            if hash.is_empty() || hash.len() > 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                String::from("unknown")
            } else {
                hash.to_string()
            }
        }
        _ => String::from("unknown"),
    }
}
