//! Kitty backend call-site fence (CTX-1614, #2163).
//!
//! The runtime reaches Kitty decode and raster mechanics only through the
//! injected dyn seam ([`bitty_rich::kitty_seam::KittyBackendHandle`]). This
//! gate scans `crates/bitty-runtime/src` code for the in-repo free functions
//! and for concrete [`bitty_rich::kitty_seam::CoreKittyBackend`] construction,
//! and fails on either outside the two default-handle installations: a call
//! site that bypasses the handle would silently ignore the composition-root
//! binding and keep using Core's copy after the extension backend lands.
//!
//! The scan is token-aware: doc/line comments are stripped only outside
//! literals, block comments nest, and string/raw-string/char bodies are
//! blanked, so a `//` inside a URL string cannot swallow the call after it
//! and an identifier inside a log message never reads as a call site.
//!
//! Allowed naming points are the seam module itself
//! (`crates/bitty-rich/src/kitty_seam.rs`, whose `CoreKittyBackend` is the
//! in-tree implementation), the mechanics modules behind it, their tests,
//! and the runtime's default handle constructors. The scan is deliberately
//! local: it covers the consumer crate, not the implementation crate.

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

/// The only legal concrete-backend construction: the default handle installed
/// by each runtime constructor. Whitespace is compacted before comparison.
const DEFAULT_HANDLE_INIT: &str =
    "kitty_graphics:std::sync::Arc::new(bitty_rich::kitty_seam::CoreKittyBackend::new()),";

/// Runtime source root: `<workspace>/crates/bitty-runtime/src`.
fn runtime_source_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/<name> parent layout");
    workspace.join("crates/bitty-runtime/src")
}

fn compact(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Violations in one source text, as `line N: reason: code` strings.
///
/// Two rules: a forbidden free function on a code line, and any mention of
/// the concrete backend outside [`DEFAULT_HANDLE_INIT`]. The latter keeps
/// `CoreKittyBackend::new().decode_transmit(..)` in the runtime flagged even
/// though it names no free function.
fn runtime_violations(source: &str) -> Vec<String> {
    let mut violations = Vec::new();
    for (offset, line) in code_view(source).lines().enumerate() {
        let code = line.trim();
        if code.is_empty() {
            continue;
        }
        let code = compact(code);
        for name in FORBIDDEN_FREE_FUNCTIONS {
            if code.contains(name) {
                violations.push(format!("line {}: free function `{name}`", offset + 1));
            }
        }
        if code.contains("CoreKittyBackend") && code != DEFAULT_HANDLE_INIT {
            violations.push(format!(
                "line {}: concrete backend construction",
                offset + 1
            ));
        }
    }
    violations
}

/// Comment-free, literal-free view of Rust source, with newlines preserved.
///
/// Line comments stop outside literals; block comments nest; normal strings,
/// raw strings, and char literals are blanked (their `//`, `/*`, or identifier
/// text must never affect the scan). Lifetimes (`'a`) stay code: only a
/// closed char literal is consumed.
fn code_view(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '/' && chars.get(index + 1) == Some(&'*') {
            index = skip_block_comment(&chars, index, &mut out);
            continue;
        }
        if ch == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if let Some((quote, hashes)) = raw_string_start(&chars, index) {
            if let Some(end) = raw_string_end(&chars, quote, hashes) {
                copy_newlines(&chars[index..=end], &mut out);
                out.push(' ');
                index = end + 1;
                continue;
            }
        }
        if ch == '"' {
            if let Some(end) = quoted_string_end(&chars, index) {
                copy_newlines(&chars[index..=end], &mut out);
                out.push(' ');
                index = end + 1;
                continue;
            }
        }
        if ch == '\'' {
            if let Some(end) = char_literal_end(&chars, index) {
                copy_newlines(&chars[index..=end], &mut out);
                out.push(' ');
                index = end + 1;
                continue;
            }
        }
        out.push(ch);
        index += 1;
    }
    out
}

/// Skips one nested block comment starting at `start`, emitting its newlines.
/// Returns the index just past the closing `*/` (or the end of input).
fn skip_block_comment(chars: &[char], start: usize, out: &mut String) -> usize {
    let mut index = start + 2;
    let mut depth = 1usize;
    while index < chars.len() {
        if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
            depth += 1;
            index += 2;
        } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
            depth -= 1;
            index += 2;
            if depth == 0 {
                out.push(' ');
                return index;
            }
        } else {
            if chars[index] == '\n' {
                out.push('\n');
            }
            index += 1;
        }
    }
    out.push(' ');
    index
}

fn copy_newlines(slice: &[char], out: &mut String) {
    for &ch in slice {
        if ch == '\n' {
            out.push('\n');
        }
    }
}

/// Closing quote index of a normal string starting at `start` (`"`).
fn quoted_string_end(chars: &[char], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < chars.len() {
        match chars[index] {
            '\\' => index += 2,
            '"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// Raw string opener (`r"`, `r#"`, `br#"`, `cr#"`):
/// `(quote index, hash count)`.
fn raw_string_start(chars: &[char], index: usize) -> Option<(usize, usize)> {
    let mut cursor = match *chars.get(index)? {
        'r' => index + 1,
        'b' | 'c' if chars.get(index + 1) == Some(&'r') => index + 2,
        _ => return None,
    };
    let mut hashes = 0;
    while chars.get(cursor) == Some(&'#') {
        hashes += 1;
        cursor += 1;
    }
    // `r#foo` is a raw identifier, not a literal: only a quote opens one.
    (chars.get(cursor) == Some(&'"')).then_some((cursor, hashes))
}

/// Closing quote index of a raw string with `hashes` terminator hashes.
fn raw_string_end(chars: &[char], quote: usize, hashes: usize) -> Option<usize> {
    let mut cursor = quote + 1;
    while cursor < chars.len() {
        if chars[cursor] == '"'
            && (1..=hashes).all(|offset| chars.get(cursor + offset) == Some(&'#'))
        {
            return Some(cursor + hashes);
        }
        cursor += 1;
    }
    None
}

/// Closing quote index of a char literal starting at `start` (`'`).
///
/// Returns `None` for lifetimes and labels (`'a`, `'outer:`), which stay code.
fn char_literal_end(chars: &[char], start: usize) -> Option<usize> {
    let next = *chars.get(start + 1)?;
    if next == '\\' {
        // Skip the escape and its escaped character (`'\''`, `'\n'`, `'\u{..}'`).
        let mut index = start + 3;
        while index < chars.len() {
            match chars[index] {
                '\'' => return Some(index),
                '\n' => return None,
                _ => index += 1,
            }
        }
        return None;
    }
    (chars.get(start + 2) == Some(&'\'')).then_some(start + 2)
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
        for violation in runtime_violations(&source) {
            violations.push(format!("{}: {violation}", path.display()));
        }
    }
    assert!(
        violations.is_empty(),
        "runtime Kitty call sites must dispatch through the injected seam, \
         never the free functions or a concrete backend: {violations:#?}"
    );
}

#[test]
fn runtime_seam_binding_is_the_dyn_handle() {
    // Absence alone would also pass if every call site disappeared; pin the
    // binding shape, the default installs, and the rebind invalidation.
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
    let default_bindings = code_view(&runtime)
        .lines()
        .filter(|line| compact(line) == DEFAULT_HANDLE_INIT)
        .count();
    assert_eq!(
        default_bindings, 2,
        "both runtime constructors must install the default in-tree handle"
    );
    let decodes = compact(&kitty_images);
    assert!(
        decodes.contains("self.kitty_graphics.decode_transmit"),
        "runtime/kitty_images.rs must decode through the injected handle"
    );
    assert!(
        decodes.contains("self.invalidate_kitty_raster_cache();self.kitty_graphics=backend;"),
        "set_kitty_backend must drop cached blits before installing the new handle"
    );
    let rasters = compact(&present);
    assert!(
        rasters.contains("self.kitty_graphics.rasterize_frame"),
        "runtime/present.rs must rasterize through the injected handle"
    );
}

#[test]
fn fence_scanner_flags_direct_and_concrete_use() {
    for probe in [
        "let image = decode_kitty_payload(100, None, None, PAYLOAD);",
        "let image = decode_kitty_payload_owned(32, Some(2), Some(2), payload);",
        "let rgba = rasterize_kitty(&image, rect);",
        "let rgba = rasterize_kitty_clipped(&image, full, visible);",
        "let rgba = bitty_rich::rasterize_rgba_clipped(w, h, bytes, full, visible);",
        // A concrete backend bypasses the handle even without a free function.
        "bitty_rich::kitty_seam::CoreKittyBackend::new().decode_transmit(f, None, None, p);",
        // `//` inside a string must not swallow the call after it.
        "let url = \"https://example.test\"; let image = decode_kitty_payload(100, None, None, P);",
        // A char-literal quote must not open a string that hides the call.
        "let quote = '\"'; let image = decode_kitty_payload(100, None, None, P);",
        // A block comment must not hide the call that follows it.
        "let ok = 1; /* rasterize_kitty */ let image = decode_kitty_payload(100, None, None, P);",
    ] {
        assert!(
            !runtime_violations(probe).is_empty(),
            "direct/concrete use must be flagged: {probe}"
        );
    }
    for allowed in [
        "// a comment may name decode_kitty_payload",
        "/// #2163: the seam replaced [`rasterize_kitty`]",
        "//! module docs may link rasterize_rgba_clipped",
        "",
        "let s = \"decode_kitty_payload\";",
        "let s = r#\"// not a comment /* nor this */ decode_kitty_payload\"#;",
        "/* outer /* nested rasterize_kitty */ still a comment */",
        "/*\ndecode_kitty_payload\n*/\nlet ok = 1;",
        "fn f<'a>(x: &'a str) -> &'a str { x }",
        "let rgba = self.kitty_graphics.rasterize_frame(w, h, bytes, full, visible);",
        "let image = self.kitty_graphics.decode_transmit(format, None, None, payload);",
        DEFAULT_HANDLE_INIT,
    ] {
        assert!(
            runtime_violations(allowed).is_empty(),
            "comments, literals, lifetimes, and seam dispatch must stay allowed: {allowed}"
        );
    }
}
