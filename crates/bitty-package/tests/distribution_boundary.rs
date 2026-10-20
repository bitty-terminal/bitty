//! Distribution-boundary guard for bitty#1999 (core-split).
//!
//! The accepted W-72 boundary assigns dependency resolution to the external
//! `bitty-plugin-manager`; Core retains only manifest/lock parsing, integrity
//! verification, startup re-verification, and the in-memory lifecycle
//! mechanism. bitty#1999 retired Core's draft resolver
//! (`crates/bitty-package/src/resolver.rs`) because no Core path consumed it.
//!
//! This guard closes the W-147 fence's source-level blind spot for that
//! retirement: the `Cargo.toml` edge gate stays green when Core re-adds
//! duplicate logic without a dependency, so the retired module path and its
//! public symbols are rejected here directly.
//!
//! Mutation proof (recorded in the PR): re-creating
//! `crates/bitty-package/src/resolver.rs` with any retired symbol makes
//! [`resolver_module_is_retired_from_core`] fail; deleting it makes the guard
//! pass again.

use std::path::{Path, PathBuf};

use bitty_package::{OperationOwner, PackageOperation};

/// Exact module path retired from Core by bitty#1999.
const RETIRED_MODULE_PATH: &str = "crates/bitty-package/src/resolver.rs";

/// Identifiers that only the retired dependency resolver exposed. `resolve`
/// and `Resolution` are deliberately absent: both are too generic and appear
/// in unrelated Core code.
const RETIRED_IDENTIFIERS: &[&str] = &[
    "PackageIndex",
    "IndexEntry",
    "ResolvedPackage",
    "MAX_SOLVER_STEPS",
    "MAX_CANDIDATES_PER_PACKAGE",
    "MAX_EDGES_PER_PACKAGE",
    "MAX_PACKAGES_PER_RESOLUTION",
    "resolve_preserving_locked",
];

/// Workspace root derived from this crate's manifest dir (no hardcoded
/// checkout paths): `<workspace>/crates/bitty-package` -> `<workspace>`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> parent layout")
        .to_path_buf()
}

/// Workspace member crate names parsed from the root `Cargo.toml` `members`
/// array (quoted `"crates/<name>"` entries only), mirroring the W-147 gate.
fn workspace_members(root: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");
    let members_section = text
        .split("members = [")
        .nth(1)
        .expect("workspace Cargo.toml must declare members");
    let list = members_section
        .split(']')
        .next()
        .expect("members array must close");
    list.lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_matches([',', ' ']);
            let inner = trimmed.strip_prefix('"')?.strip_suffix('"')?;
            inner
                .strip_prefix("crates/")
                .map(std::string::ToString::to_string)
        })
        .collect()
}

/// Collect `*.rs` files under `dir` (recursive, skips `target/`).
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|err| panic!("read dir {}: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.file_name().is_some_and(|name| name == "target") {
            continue;
        }
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// True when Rust source names `ident` as a whole token (catches
/// `ident::...`, `use ident;`, and declarations without `::`).
fn names_identifier(text: &str, ident: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == ident)
}

#[test]
fn resolver_module_is_retired_from_core() {
    let root = workspace_root();
    assert!(
        !root.join(RETIRED_MODULE_PATH).exists(),
        "retired distribution module must stay absent: {RETIRED_MODULE_PATH}"
    );

    let members = workspace_members(&root);
    let mut module_declarations = Vec::new();
    let mut symbol_offenders = Vec::new();
    for member in &members {
        let src = root.join("crates").join(member).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read rs file");
            // Only `bitty-package` owned the retired package resolver. A
            // differently-named resolver elsewhere (for example a Lua module
            // resolver) is not this guard's concern; the retired symbols are
            // still rejected across every Core source below.
            if *member == "bitty-package" && text.contains("mod resolver;") {
                module_declarations.push(file.clone());
            }
            for ident in RETIRED_IDENTIFIERS {
                if names_identifier(&text, ident) {
                    symbol_offenders.push(format!("{}: {ident}", file.display()));
                }
            }
        }
    }

    assert!(
        module_declarations.is_empty(),
        "Core must not declare a resolver module (W-72 manager-owned): {module_declarations:?}"
    );
    assert!(
        symbol_offenders.is_empty(),
        "Core must not reintroduce retired dependency-solving symbols: {symbol_offenders:?}"
    );
}

#[test]
fn dependency_resolution_stays_manager_owned() {
    assert_eq!(
        PackageOperation::DependencyResolution.owner(),
        OperationOwner::ExternalManager,
        "W-72 assigns dependency resolution to bitty-plugin-manager"
    );
    assert!(
        !PackageOperation::DependencyResolution.is_core_retained(),
        "Core must not perform dependency resolution"
    );
}
