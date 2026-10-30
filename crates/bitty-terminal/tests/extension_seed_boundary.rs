//! Seed-boundary guard for bitty#2399 (`W-72`).
//!
//! The extension install seed is bootstrap-only: exactly one pinned release
//! per invocation, with the release `SHA256SUMS` as the lock record. Catalog
//! resolution, latest selection, and list/inspect presentation belong to
//! `bitty-plugin-manager`; Core must not fetch or parse
//! `extensions/catalog.v1` again.
//!
//! This guard closes the source-level blind spot that behavior tests leave
//! open: a re-added catalog path with no caller, or a latest selector hidden
//! behind another name, would otherwise pass a green suite. The scan covers
//! every Rust source under `src/`, so a retired symbol cannot hide in a
//! module the seed no longer imports. Comments are stripped before matching,
//! so prose that names a retired symbol cannot false-fail the guard while
//! actual code still does; literals are copied verbatim first, so a `//` or
//! `/*` inside a string cannot hide the code after it.
//!
//! Mutation proof (recorded in the PR): re-adding any retired symbol below in
//! any `src/**/*.rs` file, or a third `transport.fetch` site in the seed,
//! fails the guard; deleting the whole catalog module keeps it green.

use std::path::{Path, PathBuf};

/// Extension seed source files that must stay pinned-only and off the Rust
/// network stack. Retired-identifier scanning covers all of `src/`.
const SEED_SOURCES: &[&str] = &["src/component_seed.rs", "src/component.rs"];

/// Identifiers that only a Core-owned catalog/latest path would expose.
const RETIRED_IDENTIFIERS: &[&str] = &[
    "ExtensionCatalog",
    "CatalogFetch",
    "CatalogUnavailable",
    "parse_catalog",
    "fetch_catalog",
    "latest_version_for",
    "resolve_latest_entry",
    "LatestResolveError",
    "ask_latest_component_install_consent",
    "fetch_seed_payload_with_registry",
    "is_dual_digest_verified",
    "SEED_CATALOG_KEY",
    "SEED_CATALOG_URL",
    "SEED_CATALOG_SCHEMA",
    "SEED_CATALOG_MAX_BYTES",
    "SEED_CATALOG_MAX_LINES",
    "SEED_CATALOG_LINE_MAX_BYTES",
    "SEED_CATALOG_MAX_ENTRIES",
];

/// The one documented seed budget constant.
const SEED_BUDGET: &str = "pub const SEED_MAX_RELEASE_FETCHES: usize = 1;";

/// This crate's source root (`crates/bitty-terminal`).
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Recursively collect every `.rs` file under `dir`, relative to `root`.
///
/// Sorted for a deterministic failure report. No symlinked directories are
/// followed: the repository tree is the only trusted scan input.
fn rust_sources(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            rust_sources(&path, root, out);
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            if let Ok(relative) = path.strip_prefix(root) {
                out.push(relative.to_path_buf());
            }
        }
    }
}

/// Remove line and (nested) block comments so documentation mentions do not
/// read as code.
///
/// Literals are copied verbatim *before* comment stripping, because a `//` or
/// `/*` inside one is body text, not a comment start: without that, a
/// `"https://cdn.bitty.run"` or an `"extensions/*"` literal would hide code
/// after it from the scan. Copying literals keeps the retired-identifier
/// match fail-closed: a retired symbol in a string is still suspicious enough
/// to review.
fn strip_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut block_depth = 0usize;
    while index < bytes.len() {
        if block_depth > 0 {
            if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                block_depth += 1;
                index += 2;
            } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                block_depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            block_depth = 1;
            index += 2;
            continue;
        }
        if let Some(end) = literal_end(bytes, index) {
            out.push_str(&text[index..end]);
            index = end;
            continue;
        }
        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Identifier bytes, so a literal prefix only counts at a token start.
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// End offset (exclusive) of the literal starting at `start`, if any.
///
/// Covers ordinary strings (`"…"`), byte/C strings (`b"…"`, `c"…"`), their
/// raw forms (`r"…"`, `r#"…"#`, `br##"…"##`, `cr"…"`) and char literals
/// (`'/'`, `b'"'`). Escapes are honoured, so `\"` never closes a literal.
fn literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start > 0 && is_ident_byte(bytes[start - 1]) {
        return None;
    }
    match bytes[start] {
        b'b' | b'c' => match bytes.get(start + 1) {
            Some(&b'r') => raw_string_end(bytes, start + 1),
            Some(&b'"') => quoted_end(bytes, start + 2, b'"'),
            Some(&b'\'') => char_end(bytes, start + 2),
            _ => None,
        },
        b'r' => raw_string_end(bytes, start),
        b'"' => quoted_end(bytes, start + 1, b'"'),
        b'\'' => char_end(bytes, start + 1),
        _ => None,
    }
}

/// End offset just past the closing raw-string delimiter, for the `r` at
/// `r_index`.
///
/// A raw string has no escapes: the only terminator is one `"` followed by
/// exactly the opening hash count. `r#ident` is a raw identifier, not a
/// string, and falls through to ordinary copying.
fn raw_string_end(bytes: &[u8], r_index: usize) -> Option<usize> {
    let mut cursor = r_index + 1;
    let mut hashes = 0;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            let mut closing = 0;
            while closing < hashes && bytes.get(cursor + 1 + closing) == Some(&b'#') {
                closing += 1;
            }
            if closing == hashes {
                return Some(cursor + 1 + hashes);
            }
        }
        cursor += 1;
    }
    None
}

/// End offset just past the closing `quote`, or `None` when the literal is
/// unterminated (the caller then copies byte by byte).
fn quoted_end(bytes: &[u8], mut cursor: usize, quote: u8) -> Option<usize> {
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            byte if byte == quote => return Some(cursor + 1),
            _ => cursor += 1,
        }
    }
    None
}

/// End offset just past a char literal, or `None` when the `'` opens a
/// lifetime (`&'a str`) instead.
fn char_end(bytes: &[u8], start: usize) -> Option<usize> {
    // A char literal is short (`'\u{10FFFF}'` is 12 bytes) and never spans a
    // line, while a lifetime has no closing quote at all. Bounding the scan
    // keeps `&'a` from swallowing a later literal or comment.
    let limit = bytes.len().min(start + 16);
    let mut cursor = start;
    while cursor < limit {
        match bytes[cursor] {
            b'\\' => cursor += 2,
            b'\'' => return Some(cursor + 1),
            b'\n' => return None,
            _ => cursor += 1,
        }
    }
    None
}

/// True when source code names `ident` as a whole token (catches
/// `ident::...`, `use ident;`, and declarations without `::`).
fn names_identifier(text: &str, ident: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == ident)
}

/// Retired identifiers named in `text`, in `RETIRED_IDENTIFIERS` order.
///
/// Single predicate behind both the real scan and the fixtures below, so a
/// fixture that trips here proves the guard itself rejects that source.
fn retired_identifiers_in(text: &str) -> Vec<&'static str> {
    let code = strip_comments(text);
    RETIRED_IDENTIFIERS
        .iter()
        .copied()
        .filter(|ident| names_identifier(&code, ident))
        .collect()
}

/// Reject a catalog/latest symbol anywhere in the crate sources.
#[test]
fn catalog_and_latest_stay_out_of_core() {
    let root = crate_root();
    let mut sources = Vec::new();
    rust_sources(&root.join("src"), &root, &mut sources);
    sources.sort();
    assert!(
        !sources.is_empty(),
        "the guard must find Rust sources under src/"
    );
    let mut offenders = Vec::new();
    for relative in sources {
        let path = root.join(&relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for ident in retired_identifiers_in(&text) {
            offenders.push(format!("{}: {ident}", relative.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "Core must not re-add catalog/latest seed code (bitty#2399, W-72): {offenders:?}"
    );
}

/// The seed budget is one pinned release and is documented as such.
#[test]
fn seed_budget_is_one_pinned_release() {
    let path = crate_root().join("src/component_seed.rs");
    let text = std::fs::read_to_string(&path).expect("read component_seed.rs");
    assert!(
        text.contains(SEED_BUDGET),
        "the seed budget must stay `{SEED_BUDGET}`"
    );
    assert!(
        text.contains("bitty#2399"),
        "the budget must cite the issue that capped it"
    );
    // One manifest fetch plus one tarball fetch. A catalog or metadata fetch
    // would add a third call site and fail here before review notices. Count
    // `.fetch(` rather than the one-line `transport.fetch(` spelling so a
    // rustfmt line break (the manifest call is wrapped) cannot false-pass.
    // Only the production half counts: test stubs and fixtures may call
    // `.fetch(` freely without weakening the budget.
    let production = text.split("#[cfg(test)]").next().unwrap_or(&text);
    let fetch_sites = production.matches(".fetch(").count();
    assert_eq!(
        fetch_sites, 2,
        "the seed performs exactly two production transport fetch sites (SHA256SUMS + one release), found {fetch_sites}"
    );
}

/// No seed source may open another network path (fixed-argv curl only).
#[test]
fn seed_has_no_network_stack() {
    let root = crate_root();
    for relative in SEED_SOURCES {
        let text = std::fs::read_to_string(root.join(relative)).expect("read seed source");
        for forbidden in ["reqwest", "hyper::", "TcpStream", "std::net"] {
            assert!(
                !text.contains(forbidden),
                "{relative} must stay off the Rust network stack (found {forbidden})"
            );
        }
    }
}

/// Guard fixture: the identifier scanner ignores prose-only mentions.
#[test]
fn scanner_ignores_documentation_mentions() {
    let prose = "//! The catalog stays a published artifact; no parse_catalog here.\n";
    assert!(!names_identifier(&strip_comments(prose), "parse_catalog"));
    let block = "/* parse_catalog is retired */\nfn live() {}\n";
    assert!(!names_identifier(&strip_comments(block), "parse_catalog"));
    // Lifetimes are not unterminated char literals: a `'a` must not swallow
    // the comment that follows it.
    let lifetime = "fn borrow<'a>(x: &'a str) -> &'a str { x }\n/* parse_catalog */\n";
    assert!(!names_identifier(
        &strip_comments(lifetime),
        "parse_catalog"
    ));
    assert!(names_identifier(
        &strip_comments("pub fn parse_catalog() {}"),
        "parse_catalog"
    ));
}

/// Guard fixture: a `//` inside a string literal is body text, not a comment.
///
/// Before the scanner copied literals, this line's `https://` opened a line
/// comment and hid the `fetch_catalog` definition beside it.
#[test]
fn scanner_is_not_fooled_by_line_comment_marker_in_string() {
    let source =
        "const SEED_URL: &str = \"https://cdn.bitty.run\"; fn fetch_catalog() {}\nfn live() {}\n";
    assert_eq!(
        retired_identifiers_in(source),
        vec!["fetch_catalog"],
        "a `//` inside a string literal must not hide the rest of the line"
    );
}

/// Guard fixture: a `/*` inside a string literal does not open a block
/// comment.
///
/// Before the scanner copied literals, this `"extensions/*"` glob opened a
/// block comment that never closed and hid every later definition.
#[test]
fn scanner_is_not_fooled_by_block_comment_marker_in_string() {
    let source = "const SEED_GLOB: &str = \"extensions/*\";\nfn parse_catalog() {}\n";
    assert_eq!(
        retired_identifiers_in(source),
        vec!["parse_catalog"],
        "a `/*` inside a string literal must not swallow the rest of the file"
    );
}

/// Guard fixture: escapes and raw strings are copied as literal bodies.
#[test]
fn scanner_copies_escapes_and_raw_strings() {
    let escaped = "const Q: &str = \"escaped \\\" // body\"; fn fetch_catalog() {}\n";
    assert_eq!(
        retired_identifiers_in(escaped),
        vec!["fetch_catalog"],
        "an escaped quote must not close the literal early"
    );

    let raw = "const R: &str = r#\"extensions/* // body\"#;\nfn parse_catalog() {}\n";
    assert_eq!(
        retired_identifiers_in(raw),
        vec!["parse_catalog"],
        "a raw string must be copied through its matching hash delimiter"
    );
}

/// Guard fixture: a retired identifier in real code still fails the guard.
#[test]
fn scanner_still_rejects_real_definitions() {
    let source = "use crate::catalog::fetch_catalog;\nfn parse_catalog() {}\n";
    assert_eq!(
        retired_identifiers_in(source),
        vec!["parse_catalog", "fetch_catalog"],
        "the literal handling must not weaken the retired-identifier match"
    );
}
