//! Binary artifact name guard for CTX-0164 (#264).
//!
//! The `bitty-app` crate must produce a binary named `bitty` (not
//! `bitty-app`) so `target/debug/bitty`, `ps`, and fastfetch agree with
//! `/usr/bin/bitty`. Cargo exposes each binary to integration tests via
//! `CARGO_BIN_EXE_<name>`; referencing `CARGO_BIN_EXE_bitty` fails to
//! compile if the `[[bin]]` rename regresses.

/// Compile-time proof that the `[[bin]]` artifact is named `bitty`.
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

#[test]
fn binary_artifact_is_named_bitty() {
    let file = std::path::Path::new(BITTY_BIN)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    // Windows appends `.exe`; Unix is exactly `bitty`.
    assert!(
        file == "bitty" || file == "bitty.exe",
        "expected binary file name `bitty`, got `{file}` from `{BITTY_BIN}`"
    );
}

#[test]
fn cargo_manifest_declares_bin_bitty() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).expect("read bitty-app Cargo.toml");
    assert!(
        text.contains("[[bin]]"),
        "crates/bitty-app/Cargo.toml must contain a [[bin]] section"
    );
    assert!(
        text.contains("name = \"bitty\""),
        "crates/bitty-app/Cargo.toml [[bin]] must set name = \"bitty\""
    );
    // Crate itself keeps the `bitty-app` name; only the artifact renames.
    assert!(
        text.contains("name = \"bitty-app\""),
        "crate package name must stay `bitty-app`"
    );
}
