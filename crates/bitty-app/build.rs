//! Build-time version metadata for `bitty version`.
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
