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
//! behind another name, would otherwise pass a green suite. Mutation proof
//! (recorded in the PR): re-adding any retired symbol below, or a third
//! `transport.fetch` site, fails the guard; deleting the whole catalog module
//! keeps it green.

use std::path::{Path, PathBuf};

/// Extension seed source files that must stay pinned-only.
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

/// True when Rust source names `ident` as a whole token (catches
/// `ident::...`, `use ident;`, and declarations without `::`).
fn names_identifier(text: &str, ident: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == ident)
}

/// Reject a catalog/latest symbol anywhere in the seed sources.
#[test]
fn catalog_and_latest_stay_out_of_core() {
    let root = crate_root();
    let mut offenders = Vec::new();
    for relative in SEED_SOURCES {
        let path = root.join(relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for ident in RETIRED_IDENTIFIERS {
            if names_identifier(&text, ident) {
                offenders.push(format!("{relative}: {ident}"));
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
    let fetch_sites = text.matches(".fetch(").count();
    assert_eq!(
        fetch_sites, 2,
        "the seed performs exactly two transport fetch sites (SHA256SUMS + one release), found {fetch_sites}"
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
    let prose = "//! The catalog stays a published artifact; no fetch_catalog here.\n";
    assert!(!names_identifier(prose, "parse_catalog"));
    assert!(names_identifier(
        "pub fn parse_catalog() {}",
        "parse_catalog"
    ));
}

/// Keep the helper honest across platforms; `Path` is referenced by
/// `crate_root`'s return type on every target.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
