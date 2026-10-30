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
//! actual code still does.
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
/// read as code. String literals are left intact deliberately: a retired
/// identifier in a string is still suspicious enough to review.
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
        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// True when source code names `ident` as a whole token (catches
/// `ident::...`, `use ident;`, and declarations without `::`).
fn names_identifier(text: &str, ident: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == ident)
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
        let code = strip_comments(&text);
        for ident in RETIRED_IDENTIFIERS {
            if names_identifier(&code, ident) {
                offenders.push(format!("{}: {ident}", relative.display()));
            }
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
    assert!(names_identifier(
        &strip_comments("pub fn parse_catalog() {}"),
        "parse_catalog"
    ));
}
