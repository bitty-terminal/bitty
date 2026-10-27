//! Kitty backend call-site fence (CTX-1614, #2163).
//!
//! The runtime reaches Kitty decode and raster mechanics only through the
//! injected dyn seam ([`bitty_rich::kitty_seam::KittyBackendHandle`]). This
//! gate scans `crates/bitty-runtime/src` code lines for the in-repo free
//! functions and fails on any direct name: a call site that bypasses the
//! handle would silently ignore the composition-root binding and keep using
//! Core's copy after the extension backend lands.
//!
//! Allowed naming points are the seam module itself
//! (`crates/bitty-rich/src/kitty_seam.rs`, whose `CoreKittyBackend` is the
//! in-tree implementation), the mechanics modules behind it, their tests,
//! and doc comments (links, not calls). The scan is deliberately local: it
//! covers the consumer crate, not the implementation crate.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Free functions the runtime must never call directly.
///
/// `decode_kitty_payload` is a prefix of `decode_kitty_payload_owned` and
/// `rasterize_kitty` a prefix of `rasterize_kitty_clipped`; a plain substring
/// test therefore catches both spellings from one entry.
const FORBIDDEN_FREE_FUNCTIONS: &[&str] = &[
    "decode_kitty_payload",
    "rasterize_kitty",
    "rasterize_rgba_clipped",
];

/// Runtime source root: `<workspace>/crates/bitty-runtime/src`.
fn runtime_source_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/<name> parent layout");
    workspace.join("crates/bitty-runtime/src")
}

/// Code-line references (line comments stripped) to a forbidden free
/// function. Doc comments (`///`, `//!`) and plain comments are stripped with
/// the same prefix rule, so links in documentation stay allowed.
fn free_function_references(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let code = line.split("//").next().unwrap_or("").trim();
            if code.is_empty() {
                return None;
            }
            FORBIDDEN_FREE_FUNCTIONS
                .iter()
                .any(|name| code.contains(name))
                .then(|| code.to_string())
        })
        .collect()
}

/// Every `.rs` file under `root`, recursively (no external crate).
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

#[test]
fn runtime_call_sites_never_name_the_free_functions() {
    let root = runtime_source_root();
    let mut violations = Vec::new();
    for path in rust_sources(&root) {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        for reference in free_function_references(&source) {
            violations.push(format!("{}: {reference}", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "runtime Kitty call sites must dispatch through the injected seam, \
         never the free functions: {violations:#?}"
    );
}

/// Whitespace-free code view (line comments stripped), so a dispatch split
/// across lines still matches its single-line spelling.
fn compact_code(source: &str) -> String {
    source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<String>()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

#[test]
fn runtime_seam_binding_is_the_dyn_handle() {
    // Absence alone would also pass if every call site disappeared; pin the
    // binding shape and its consumers as well.
    let root = runtime_source_root();
    let runtime = std::fs::read_to_string(root.join("runtime.rs")).expect("read runtime.rs");
    let kitty_images = std::fs::read_to_string(root.join("runtime/kitty_images.rs"))
        .expect("read runtime/kitty_images.rs");
    let present =
        std::fs::read_to_string(root.join("runtime/present.rs")).expect("read runtime/present.rs");
    assert!(
        runtime.contains("kitty_graphics: bitty_rich::kitty_seam::KittyBackendHandle"),
        "the runtime field must stay the dyn seam handle"
    );
    assert!(
        runtime.contains("pub fn set_kitty_backend")
            || kitty_images.contains("pub fn set_kitty_backend"),
        "the composition-root setter must stay on the runtime"
    );
    let decodes = compact_code(&kitty_images);
    assert!(
        decodes.contains("self.kitty_graphics.decode_transmit"),
        "runtime/kitty_images.rs must decode through the injected handle"
    );
    let rasters = compact_code(&present);
    assert!(
        rasters.contains("self.kitty_graphics.rasterize_frame"),
        "runtime/present.rs must rasterize through the injected handle"
    );
}

#[test]
fn free_function_scanner_flags_calls_and_allows_comments() {
    for probe in [
        "let image = decode_kitty_payload(100, None, None, PAYLOAD);",
        "let image = decode_kitty_payload_owned(32, Some(2), Some(2), payload);",
        "let rgba = rasterize_kitty(&image, rect);",
        "let rgba = rasterize_kitty_clipped(&image, full, visible);",
        "let rgba = bitty_rich::rasterize_rgba_clipped(w, h, bytes, full, visible);",
    ] {
        assert!(
            !free_function_references(probe).is_empty(),
            "direct free-function use must be flagged: {probe}"
        );
    }
    for allowed in [
        "// a comment may name decode_kitty_payload",
        "/// #2163: the seam replaced [`rasterize_kitty`]",
        "//! module docs may link rasterize_rgba_clipped",
        "",
        "let rgba = self.kitty_graphics.rasterize_frame(w, h, bytes, full, visible);",
        "let image = self.kitty_graphics.decode_transmit(format, None, None, payload);",
    ] {
        assert!(
            free_function_references(allowed).is_empty(),
            "comments and seam dispatch must stay allowed: {allowed}"
        );
    }
}
