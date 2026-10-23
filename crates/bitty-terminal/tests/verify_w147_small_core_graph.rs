//! W-147 final verification gate: small-core dependency graph + safe startup
//! (CTX-0940, issue #1621; extended to every extracted extension crate by
//! CTX-1364 / #2256).
//!
//! These tests pin the program end-state so a later change cannot silently
//! re-grow the retired surface:
//!
//! - validation suites (`bitty-compat-lab`, `bitty-perf`) live outside the
//!   workspace (W-105, CTX-0931): no workspace member, no normal dependency
//!   edge;
//! - harness-only crates (`bitty-test-support`, `bitty-test-vm`) are
//!   dev-dependencies only: no production (`[dependencies]`) edge anywhere;
//! - every extracted extension crate is fenced. Implementation crates
//!   (`bitty-storage`, `bitty-execution`, `bitty-graphics`,
//!   `bitty-agent`, `bitty-agent-api`, `bitty-observability`,
//!   `bitty-observability-core`, `bitty-network`, `bitty-net`) must have no
//!   normal dependency edge and no Core source identifier; `bitty-storage` is
//!   the single composition-root mechanics seam, linked only by
//!   `bitty-terminal` and named only in `storage_backends.rs` plus the
//!   default-off component host and its hermetic fixture
//!   (`crates/bitty-terminal/src/storage_component_host.rs`,
//!   `crates/bitty-terminal/src/bin/bitty-storage-component-fixture.rs`).
//!   `bitty-a11y` is the deliberate accessibility snapshot-model/component
//!   seam (CTX-1362 / CTX-1466 / #2257 / #2496): linked only by `bitty-ui` and
//!   named only in `crates/bitty-ui/src/a11y_host.rs` (`SOURCE_SEAM_FILES`).
//!   `bitty-execution` is the Core-runtime execution mechanic seam
//!   (CTX-1403 / #2249, host client #2494): linked only by `bitty-runtime`
//!   and named only in `execution_jobs.rs`, `execution_component/`, and its
//!   fixture binary (`SOURCE_SEAM_FILES`).
//!   Seam crates
//!   (`bitty-ipc`, `bitty-ipc-api`, `bitty-network-wire`,
//!   `bitty-network-publication`, `bitty-observability-api`,
//!   `bitty-platform-services`, `bitty-url-detector`) are
//!   contract/codec/transport seams Core is expected to link; `SEAM_CRATES`
//!   records the exact consumers allowed and Core may name them by design;
//! - a new direct git dependency from an extension repository fails
//!   `every_direct_extension_dependency_is_fenced` until its crate is added to
//!   `EXTENSION_CRATES`;
//! - `bitty --safe` starts with zero third-party plugins: the safe load
//!   policy selects nothing hostile, and the built binary skips a hostile
//!   dev-root plugin with no VM.
//!
//! Source-level vs manifest-level: the `Cargo.toml` gate
//! (`extension_crate_edges_match_fence_policy`) proves every dependency edge
//! matches its fence policy, but it stays green when Core duplicates
//! extension logic without declaring a dependency (audit CTX-1085 F10: kitty
//! decode/raster, process supervisors, a11y model). The
//! `core_library_sources_never_name_extension_impls` scan adds a
//! source-reference fence by rejecting every implementation identifier in Core
//! library sources. It does not detect copied logic that contains none of
//! these identifiers.
//!
//! The graphics seam (#2163) is the one source-restricted crate with no
//! production edge: `crates/bitty-rich/src/kitty_seam.rs` is the designated
//! naming point for the decode/raster contract. The composition-root binding
//! has no separate open task; the remaining extension-side gate is
//! `bitty-graphics#17` (the packed component is a version-only stub), and
//! Core tracks the remainder in #2163. No Core **source** names the crate
//! today; manifests and the crate README still name it by design. The
//! identifier scan, the crate-name scan, and the mutation tests all reject a
//! direct import, module alias, or prose mention outside that module, and the
//! crate-root seam-reference guard keeps the seam reachable only through
//! `bitty_rich::kitty_seam`.
//!
//! Graph evidence beyond assertion: `cargo tree -e normal` (recorded in the
//! CTX-0940 verification report) shows `bitty-storage` reachable only from
//! `bitty-terminal`, and `cargo tree -i bitty-test-support/-test-vm -e normal`
//! shows no normal-edge consumers.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Binary under test (fails to compile if the `[[bin]]` rename regresses).
const BITTY_BIN: &str = env!("CARGO_BIN_EXE_bitty");

/// Crates that must never appear on a production edge after W-105: the
/// relocated validation suites plus the harness-only crates.
const TEST_ONLY_CRATES: &[&str] = &[
    "bitty-compat-lab",
    "bitty-perf",
    "bitty-test-support",
    "bitty-test-vm",
];

/// Every extracted extension crate covered by the W-147 fence. Crates absent
/// from `SEAM_CRATES` are implementation crates: no normal dependency edge and
/// no Core source identifier. `bitty-storage` stays first because it is the
/// only implementation crate linked by the composition root
/// (`bitty-terminal`), and only `storage_backends.rs` may name it.
///
/// Crates in `SEAM_CRATES` are contract/codec seams Core is expected to link;
/// that table records the exact allowed consumers. Adding a direct extension
/// dependency fails `every_direct_extension_dependency_is_fenced` until the
/// crate is listed here.
const EXTENSION_CRATES: &[&str] = &[
    "bitty-storage",
    "bitty-execution",
    "bitty-graphics",
    "bitty-a11y",
    "bitty-agent",
    "bitty-agent-api",
    "bitty-observability",
    "bitty-observability-core",
    "bitty-network",
    "bitty-net",
    "bitty-ipc",
    "bitty-ipc-api",
    "bitty-network-wire",
    "bitty-network-publication",
    "bitty-observability-api",
    "bitty-platform-services",
    "bitty-url-detector",
];

/// Composition-root seam crates and the exact workspace members allowed to
/// declare a normal dependency on each. These carry contracts or codecs, not
/// extension mechanics, so Core naming them is expected. Every crate in
/// `EXTENSION_CRATES` that is absent here must have no normal edge at all.
const SEAM_CRATES: &[(&str, &[&str])] = &[
    (
        // CTX-1412 / bitty#2117: `bitty-rich` no longer links IPC. Its only
        // consumer was the retired fragment-to-`RichBlock` projection
        // (`src/projection.rs`), so the allowlist narrows to the runtime and
        // the composition root.
        "bitty-ipc",
        &["bitty-runtime", "bitty-terminal"],
    ),
    ("bitty-network-wire", &["bitty-runtime"]),
    // Feature-gated provider-publication control-record contract (CTX-1390,
    // bitty#2322): intentionally linked by the runtime, default-off.
    ("bitty-ipc-api", &["bitty-runtime"]),
    // Feature-gated publication-control transport (CTX-1373, bitty#2199):
    // intentionally linked by the runtime, default-off.
    ("bitty-network-publication", &["bitty-runtime"]),
    ("bitty-observability-api", &["bitty-terminal"]),
    ("bitty-platform-services", &["bitty-runtime"]),
    ("bitty-url-detector", &["bitty-runtime"]),
    // CTX-1362 / CTX-1466 / #2257 / #2496: the accepted W-134 snapshot-model
    // and default-off component protocol seam. Both pins are consumed by
    // `bitty-ui`'s host module; neither grants a platform backend or action
    // authority. Unlike the other seams, source naming is restricted to one
    // file by `SOURCE_SEAM_FILES`.
    ("bitty-a11y", &["bitty-ui"]),
    // CTX-1403 / #2249: the Core-runtime execution mechanic seam. It is an
    // implementation crate, not a contract/codec seam, but it has the same
    // exact-consumer + single-source-file shape: `bitty-runtime` owns the
    // tool/plugin spawn surface and only `execution_jobs.rs` may name it.
    ("bitty-execution", &["bitty-runtime"]),
];

/// Graphics decode/raster seam module (#2163). It is the single Core **source**
/// allowed to name the graphics extension crate; manifests and crate docs
/// still name it by design. The crate has no production edge: the
/// composition-root binding has no separate open task, the remaining
/// extension-side gate is `bitty-graphics#17` (version-only stub), and Core
/// tracks the remainder in #2163. This module is a designated contract naming
/// point rather than an import site.
const GRAPHICS_SOURCE_SEAM: &str = "crates/bitty-rich/src/kitty_seam.rs";
/// Crates whose Core source references are restricted to explicit files.
///
/// `bitty-a11y` is a model/protocol seam, not a general-purpose contract Core
/// anywhere: exactly `crates/bitty-ui/src/a11y_host.rs` implements
/// `SnapshotHost` and the default-off component validator, and every other
/// source imports the role vocabulary through that module's re-export.
/// `bitty-execution` is confined to the
/// `bitty-runtime` bridge (`execution_jobs.rs`, #2249), the #2494 host client
/// (`execution_component/`), and its hermetic fixture binary.
/// `bitty-graphics` has no dependency edge but is restricted to
/// `GRAPHICS_SOURCE_SEAM`; its composition-root binding has no separate open
/// task, with `bitty-graphics#17` as the extension-side gate and #2163 as the
/// Core tracker.
/// `bitty-storage` is the composition-root mechanics seam: the linked adapter
/// (`storage_backends.rs`) and the default-off component host
/// (`storage_component_host.rs`) plus its hermetic fixture binary are the only
/// files allowed to name either storage pin (CTX-1479 / bitty#2495).
const SOURCE_SEAM_FILES: &[(&str, &[&str])] = &[
    ("bitty-a11y", &["crates/bitty-ui/src/a11y_host.rs"]),
    (
        "bitty-execution",
        &[
            "crates/bitty-runtime/src/execution_jobs.rs",
            "crates/bitty-runtime/src/execution_component/mod.rs",
            "crates/bitty-runtime/src/execution_component/host.rs",
            "crates/bitty-runtime/src/bin/bitty-execution-component-fixture.rs",
        ],
    ),
    ("bitty-graphics", &[GRAPHICS_SOURCE_SEAM]),
    (
        "bitty-storage",
        &[
            "crates/bitty-terminal/src/storage_backends.rs",
            "crates/bitty-terminal/src/storage_component_host.rs",
            "crates/bitty-terminal/src/bin/bitty-storage-component-fixture.rs",
        ],
    ),
];

/// Rust identifiers for source-restricted seam crates. The component client
/// uses a Cargo alias so its unmerged protocol pin can coexist with the
/// linked model pin; that alias is fenced just as deliberately as the package
/// identifier.
const SOURCE_SEAM_IDENTIFIERS: &[(&str, &[&str])] = &[
    ("bitty-a11y", &["bitty_a11y", "bitty_a11y_component"]),
    ("bitty-execution", &["bitty_execution"]),
    ("bitty-graphics", &["bitty_graphics"]),
    (
        "bitty-storage",
        &["bitty_storage", "bitty_storage_component"],
    ),
];

/// Crates published from the bitty-terminal org that are intentionally not
/// extension crates: `phodopus` is the Lua VM linked by `bitty-lua`, not an
/// extracted extension.
const NON_EXTENSION_ORG_CRATES: &[&str] = &["phodopus"];

/// Workspace root derived from this crate's manifest dir (no hardcoded checkout
/// paths): `<workspace>/crates/bitty-terminal` -> `<workspace>`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/<name> parent layout")
        .to_path_buf()
}

/// Workspace member crate names parsed from the root `Cargo.toml` `members`
/// array (quoted `"crates/<name>"` entries only).
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

/// Raw text of one member's manifest.
fn member_manifest(root: &Path, member: &str) -> String {
    let path = root.join("crates").join(member).join("Cargo.toml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read manifest for member {member}: {err}"))
}

/// Lines of every normal (production) dependency section: `[dependencies]`,
/// `[dependencies.<name>]` tables, and `[target.<cfg>.dependencies]` overlays
/// (plus `[target.<cfg>.dependencies.<name>]` tables). Excludes
/// `[dev-dependencies]`, `[build-dependencies]`, and their `target.*`
/// counterparts, which carry no production edge. Returned lines are
/// comment-stripped so prose mentions of a crate (e.g. the W-141 note in
/// `bitty-rich`) never read as an edge. Table headers contribute a synthetic
/// `name = ...` line because their bodies (`version = ...`) never name the
/// dependency themselves.
fn normal_dependency_lines(manifest: &str) -> Vec<String> {
    let mut in_normal_deps = false;
    let mut lines = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_normal_deps = false;
            if let Some(table_dep) = normal_dep_section(trimmed) {
                in_normal_deps = true;
                if let Some(name) = table_dep {
                    lines.push(format!("{name} = {{ table header }}"));
                }
            }
            continue;
        }
        if in_normal_deps {
            let code = line.split('#').next().unwrap_or("").trim().to_string();
            if !code.is_empty() {
                lines.push(code);
            }
        }
    }
    lines
}

/// Classifies a section header: `Some(None)` for a dependency list section,
/// `Some(Some(name))` for a `[dependencies.<name>]`-style table header (the
/// edge is the table name), `None` for anything else. With
/// `include_non_production`, dev/build sections count too (dependency-crate
/// discovery); the production fence passes `false`.
fn dependency_section(header: &str, include_non_production: bool) -> Option<Option<String>> {
    let inner = header.strip_prefix('[')?.strip_suffix(']')?;
    let parts: Vec<String> = inner
        .split('.')
        .map(|part| {
            part.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string()
        })
        .collect();
    let is_dep_section = |part: &str| {
        part == "dependencies"
            || (include_non_production && matches!(part, "dev-dependencies" | "build-dependencies"))
    };
    match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [deps] if is_dep_section(deps) => Some(None),
        [deps, name] if is_dep_section(deps) => Some(Some((*name).to_string())),
        ["workspace", deps] if is_dep_section(deps) => Some(None),
        ["workspace", deps, name] if is_dep_section(deps) => Some(Some((*name).to_string())),
        ["target", .., deps] if is_dep_section(deps) => Some(None),
        ["target", .., deps, name] if is_dep_section(deps) => Some(Some((*name).to_string())),
        _ => None,
    }
}

fn normal_dep_section(header: &str) -> Option<Option<String>> {
    dependency_section(header, false)
}

fn any_dependency_section(header: &str) -> Option<Option<String>> {
    dependency_section(header, true)
}

/// True when a comment-stripped normal-dependency line declares `dep`:
/// either `name = ...` (hyphen or underscore spelling) or a renamed entry
/// such as `alias = { package = "dep", ... }`.
fn declares_dep(line: &str, dep: &str) -> bool {
    if [dep, &dep.replace('-', "_")].iter().any(|name| {
        line == *name
            || line.starts_with(&format!("{name} "))
            || line.starts_with(&format!("{name}="))
    }) {
        return true;
    }
    // Renamed entries: the line starts with the alias, the real package name
    // hides in a `package = "dep"` value (single or double quotes, any
    // spacing). Compare spaceless so `package="dep"` also matches.
    let spaceless: String = line.split_whitespace().collect();
    [dep, &dep.replace('-', "_")].iter().any(|name| {
        spaceless.contains(&format!("package=\"{name}\""))
            || spaceless.contains(&format!("package='{name}'"))
    })
}

/// Member names whose `[dependencies]` section names `dep` (hyphen or
/// underscore spelling).
fn normal_consumers_of(root: &Path, members: &[String], dep: &str) -> Vec<String> {
    members
        .iter()
        .filter(|member| {
            normal_dependency_lines(&member_manifest(root, member))
                .iter()
                .any(|line| declares_dep(line, dep))
        })
        .cloned()
        .collect()
}

/// Exact normal consumers the fence policy allows for an extension crate:
/// `bitty-terminal` for the storage composition-root seam, the table entry for
/// a seam crate, and none for every implementation crate.
fn allowed_consumers(extension: &str) -> BTreeSet<String> {
    if extension == "bitty-storage" {
        return BTreeSet::from(["bitty-terminal".to_string()]);
    }
    SEAM_CRATES
        .iter()
        .find(|(name, _)| *name == extension)
        .map(|(_, allowed)| allowed.iter().map(|name| (*name).to_string()).collect())
        .unwrap_or_default()
}

/// Manifest-level fence violations: every extension crate must have exactly
/// the normal consumers its policy allows.
fn extension_edge_violations(root: &Path) -> Vec<String> {
    let members = workspace_members(root);
    let mut violations = Vec::new();
    for extension in EXTENSION_CRATES {
        let expected = allowed_consumers(extension);
        let actual: BTreeSet<String> = normal_consumers_of(root, &members, extension)
            .into_iter()
            .collect();
        if actual != expected {
            violations.push(format!(
                "`{extension}`: expected normal consumers {expected:?}, found {actual:?}"
            ));
        }
    }
    violations
}

/// Reads a `package = "..."` rename from a dependency line or table body.
fn package_rename(line: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(offset) = line[search_from..].find("package") {
        let start = search_from + offset;
        let end = start + "package".len();
        let bounded_before = start == 0
            || !line.as_bytes()[start - 1].is_ascii_alphanumeric()
                && line.as_bytes()[start - 1] != b'_'
                && line.as_bytes()[start - 1] != b'-';
        let bounded_after = end == line.len()
            || !line.as_bytes()[end].is_ascii_alphanumeric()
                && line.as_bytes()[end] != b'_'
                && line.as_bytes()[end] != b'-';
        if bounded_before && bounded_after {
            let rest = line[end..].trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim_start();
                let quote = rest.chars().next()?;
                if quote == '"' || quote == '\'' {
                    let value_end = rest[1..].find(quote)?;
                    return Some(rest[1..1 + value_end].to_string());
                }
            }
        }
        search_from = end;
    }
    None
}

/// Direct git dependencies on `github.com/bitty-terminal/*` declared by one
/// manifest, in any dependency section (normal, dev, build, target). The
/// returned name is the package name (`package = ...` wins over an alias).
fn direct_org_git_dependencies(manifest: &str) -> Vec<String> {
    let mut in_dependencies = false;
    let mut table_crate = None;
    let mut table_package = None;
    let mut dependencies = Vec::new();
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            (in_dependencies, table_crate) = match any_dependency_section(line) {
                Some(Some(name)) => (true, Some(name)),
                Some(None) => (true, None),
                None => (false, None),
            };
            table_package = None;
            continue;
        }
        if !in_dependencies {
            continue;
        }
        if table_crate.is_some() {
            if let Some(renamed) = package_rename(line) {
                table_package = Some(renamed);
            }
        }
        if !line.contains("github.com/bitty-terminal/") || !line.contains("git") {
            continue;
        }
        let inline_name = line
            .split_once('=')
            .map(|(key, _)| key.trim())
            .filter(|key| !key.is_empty() && !key.contains(char::is_whitespace) && *key != "git");
        let name = package_rename(line)
            .or(table_package.clone())
            .or_else(|| inline_name.map(std::string::ToString::to_string))
            .or(table_crate.clone());
        if let Some(name) = name {
            dependencies.push(name);
        }
    }
    dependencies
}

#[test]
fn normal_deps_detect_table_target_and_renamed_forms() {
    // Regression for the W-147 review: production edges hide in table
    // headers, target overlays, and `package =` renames, not just
    // `[dependencies]` `name = ...` lines. Dev/build sections stay excluded.
    let manifest = r#"
[package]
name = "probe"

[dependencies.bitty-storage]
version = "0.0.1"

[dev-dependencies]
bitty-test-support = "0.0.1"

[build-dependencies]
bitty-test-vm = "0.0.1"

[target.'cfg(unix)'.dependencies]
storage = { package = "bitty-storage", version = "0.0.1" }

[target.'cfg(unix)'.dev-dependencies]
bitty-compat-lab = "0.0.1"
"#;
    let lines = normal_dependency_lines(manifest);
    assert!(
        lines.iter().any(|line| declares_dep(line, "bitty-storage")),
        "table-header and renamed target entries must read as edges: {lines:?}"
    );
    for harness in ["bitty-test-support", "bitty-test-vm", "bitty-compat-lab"] {
        assert!(
            !lines.iter().any(|line| declares_dep(line, harness)),
            "dev/build-only crate `{harness}` must not read as a normal edge: {lines:?}"
        );
    }
}

/// True when Rust source names `krate` as a crate (word-boundary match):
/// catches `krate::...` paths as well as `use krate as alias;` and
/// `extern crate krate;` forms that never spell the `::` suffix.
fn names_crate(text: &str, krate: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == krate)
}

/// True when Rust source names the hyphenated crate spelling as a token
/// (`bitty-graphics`, `bitty-graphics#15`). [`names_crate`] cannot catch it
/// because the hyphen splits the token.
fn names_crate_spelling(text: &str, krate: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        .any(|token| token == krate)
}

/// Strip the *bodies* of Rust string and char literals, keeping code,
/// comments, and line structure.
///
/// The crate-name fence catches the hyphenated spelling, which can only occur
/// in prose or metadata because a Rust identifier cannot carry a hyphen.
/// Literal contents are data, not coupling: a user-facing command example such
/// as `bitty extension install bitty-graphics` describes a CLI alias and says
/// nothing about the extension crate. Comments stay visible on purpose - doc
/// prose naming the crate is exactly what this fence exists to keep out of
/// module docs. Input the scanner cannot lex (an unterminated literal) is left
/// intact, so a lexer gap can never hide a real reference.
fn strip_literal_bodies(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut index = 0;
    while index < chars.len() {
        if let Some((quote, hashes)) = raw_string_start(&chars, index) {
            match raw_string_end(&chars, quote, hashes) {
                Some(end) => {
                    out.push(' ');
                    index = end + 1;
                }
                None => {
                    out.extend(chars[index..].iter());
                    break;
                }
            }
            continue;
        }
        if let Some(quote) = quoted_start(&chars, index) {
            match quoted_end(&chars, quote) {
                Some(end) => {
                    out.push(' ');
                    index = end + 1;
                }
                None => {
                    out.extend(chars[index..].iter());
                    break;
                }
            }
            continue;
        }
        if let Some(end) = char_literal_end(&chars, index) {
            out.push(' ');
            index = end + 1;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
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

/// Normal/byte/C string opener (`"`, `b"`, `c"`): index of the quote.
fn quoted_start(chars: &[char], index: usize) -> Option<usize> {
    match *chars.get(index)? {
        '"' => Some(index),
        'b' | 'c' if chars.get(index + 1) == Some(&'"') => Some(index + 1),
        _ => None,
    }
}

/// Closing quote index of an escaped string starting at `quote`.
fn quoted_end(chars: &[char], quote: usize) -> Option<usize> {
    let mut cursor = quote + 1;
    while cursor < chars.len() {
        match chars[cursor] {
            '\\' => cursor += 2,
            '"' => return Some(cursor),
            _ => cursor += 1,
        }
    }
    None
}

/// Closing quote index of a char/byte-char literal at `index`.
///
/// Lifetimes (`'a`, `'static`, `'_`) are deliberately *not* literals: treating
/// them as such would swallow the rest of the file and blind the fence.
fn char_literal_end(chars: &[char], index: usize) -> Option<usize> {
    let quote = match *chars.get(index)? {
        '\'' => index,
        'b' if chars.get(index + 1) == Some(&'\'') => index + 1,
        _ => return None,
    };
    if chars.get(quote + 1) == Some(&'\\') {
        let mut cursor = quote + 1;
        while cursor < chars.len() {
            match chars[cursor] {
                '\\' => cursor += 2,
                '\'' => return Some(cursor),
                _ => cursor += 1,
            }
        }
        return None;
    }
    // `'x'` is a one-char literal; `'a` is a lifetime.
    (chars.get(quote + 2) == Some(&'\'')).then_some(quote + 2)
}

#[test]
fn normal_deps_detect_target_table_header() {
    // `[target.<cfg>.dependencies.<name>]` tables carry the edge in the
    // header, like `[dependencies.<name>]`: the body never names the crate.
    let manifest = "[target.'cfg(unix)'.dependencies.bitty-storage]\nversion = \"0.0.1\"\n";
    let lines = normal_dependency_lines(manifest);
    assert!(
        lines.iter().any(|line| declares_dep(line, "bitty-storage")),
        "target table-header entry must read as an edge: {lines:?}"
    );
}

#[test]
fn crate_naming_detects_alias_and_extern_forms() {
    // Regression for the W-147 review: `use krate as alias;` and
    // `extern crate krate;` import without ever spelling `krate::`.
    assert!(names_crate(
        "use bitty_storage as storage;",
        "bitty_storage"
    ));
    assert!(names_crate("extern crate bitty_storage;", "bitty_storage"));
    assert!(names_crate(
        "let x = bitty_storage::load(&p);",
        "bitty_storage"
    ));
    assert!(!names_crate("let bitty_storage2 = 1;", "bitty_storage"));
    // Comments still match: the gate errs toward flagging (fail-closed).
    assert!(names_crate("// uses bitty_storage here", "bitty_storage"));
}

#[test]
fn workspace_members_exclude_validation_suites() {
    // W-105 (CTX-0931): compat-lab and perf moved to standalone repositories
    // pinned via `validation-pins.env`; the workspace must not re-add them.
    let root = workspace_root();
    let members = BTreeSet::from_iter(workspace_members(&root));
    for suite in ["bitty-compat-lab", "bitty-perf"] {
        assert!(
            !members.contains(suite),
            "workspace must not contain validation suite `{suite}` (W-105 relocation)"
        );
    }
}

#[test]
fn no_normal_edge_to_test_only_crates() {
    // Harness-only and relocated-suite crates may appear in
    // `[dev-dependencies]` but never on a production edge. This mirrors
    // `cargo tree -e normal`, which shows zero normal-edge consumers.
    let root = workspace_root();
    let members = workspace_members(&root);
    for dep in TEST_ONLY_CRATES {
        let consumers = normal_consumers_of(&root, &members, dep);
        assert!(
            consumers.is_empty(),
            "test-only crate `{dep}` must have no `[dependencies]` edge, found in: {consumers:?}"
        );
    }
}

#[test]
fn extension_crate_edges_match_fence_policy() {
    // W-146 DEC-W146-2 plus W-147: the composition root owns the extracted
    // storage mechanics, seam crates link only through their exact allowlist,
    // and every implementation crate has no production edge at all.
    let violations = extension_edge_violations(&workspace_root());
    assert!(
        violations.is_empty(),
        "extension fence violations: {violations:#?}"
    );
}

#[test]
fn extension_fence_lists_are_consistent() {
    // Guard the fence data itself: `EXTENSION_CRATES[0]` stays the storage
    // composition-root seam, duplicates hide policy conflicts, and every seam
    // entry must also be part of the covered crate set.
    let unique: BTreeSet<&str> = EXTENSION_CRATES.iter().copied().collect();
    assert_eq!(
        unique.len(),
        EXTENSION_CRATES.len(),
        "EXTENSION_CRATES contains duplicates"
    );
    assert_eq!(
        EXTENSION_CRATES.first().copied(),
        Some("bitty-storage"),
        "test contract: EXTENSION_CRATES[0] is the composition-root seam"
    );
    for (seam, _) in SEAM_CRATES {
        assert!(
            EXTENSION_CRATES.contains(seam),
            "seam crate `{seam}` must also be listed in EXTENSION_CRATES"
        );
    }
    for (seam, _) in SOURCE_SEAM_FILES {
        assert!(
            EXTENSION_CRATES.contains(seam),
            "source seam crate `{seam}` must also be listed in EXTENSION_CRATES"
        );
    }
}

#[test]
fn every_direct_extension_dependency_is_fenced() {
    // A new direct git dependency on a bitty-terminal extension crate must not
    // slip past the fence. Discovery is manifest-level (all dependency
    // sections, including target overlays and renamed packages) so adding a
    // crate fails here until it is classified in `EXTENSION_CRATES`.
    let root = workspace_root();
    let members = workspace_members(&root);
    let mut manifests = vec![(
        "<workspace>".to_string(),
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml"),
    )];
    manifests.extend(
        members
            .iter()
            .map(|member| (member.clone(), member_manifest(&root, member))),
    );
    let mut unfenced = Vec::new();
    for (member, manifest) in manifests {
        for dep in direct_org_git_dependencies(&manifest) {
            if NON_EXTENSION_ORG_CRATES.contains(&dep.as_str()) {
                continue;
            }
            if !EXTENSION_CRATES.contains(&dep.as_str()) {
                unfenced.push(format!("{member} -> {dep}"));
            }
        }
    }
    assert!(
        unfenced.is_empty(),
        "extension dependencies without a fence entry: {unfenced:#?}"
    );
}

/// Writes a synthetic workspace layout for the negative fence tests:
/// `<root>/Cargo.toml` members plus one manifest per member and optional
/// source files relative to the root.
fn write_probe_workspace(
    tag: &str,
    manifests: &[(&str, &str)],
    sources: &[(&str, &str)],
) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "w147-fence-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create probe workspace");
    let members: String = manifests
        .iter()
        .map(|(member, _)| format!("  \"crates/{member}\",\n"))
        .collect();
    std::fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nmembers = [\n{members}]\n"),
    )
    .expect("write probe workspace");
    for (member, manifest) in manifests {
        let dir = root.join("crates").join(member);
        std::fs::create_dir_all(&dir).expect("probe member dir");
        std::fs::write(dir.join("Cargo.toml"), manifest).expect("probe member manifest");
    }
    for (path, body) in sources {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().expect("probe source parent"))
            .expect("probe source dir");
        std::fs::write(file, body).expect("probe source");
    }
    root
}

#[test]
fn injected_manifest_edge_fails_fence() {
    // Negative test: inject one real `[dependencies]` edge for every fenced
    // crate into an otherwise unlisted workspace member. The edge scan must
    // flag each one, so a future Core dependency cannot pass unnoticed.
    let mut deps = String::new();
    for extension in EXTENSION_CRATES {
        deps.push_str(&format!("{extension} = \"0.0.1\"\n"));
    }
    let root = write_probe_workspace(
        "edge",
        &[
            ("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n"),
            (
                "bitty-untrusted",
                &format!("[package]\nname = \"bitty-untrusted\"\n\n[dependencies]\n{deps}"),
            ),
        ],
        &[],
    );
    let violations = extension_edge_violations(&root);
    let _ = std::fs::remove_dir_all(&root);
    for extension in EXTENSION_CRATES {
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(&format!("`{extension}`"))),
            "injected manifest edge `{extension}` must be rejected: {violations:#?}"
        );
    }
}

#[test]
fn injected_source_identifier_fails_fence() {
    // Negative test: a Core source naming an implementation crate identifier
    // must be rejected even with no manifest edge. Seam identifiers stay legal
    // by design, so only the implementation set is injected here.
    let seams: BTreeSet<&str> = SEAM_CRATES.iter().map(|(name, _)| *name).collect();
    let implementations: Vec<&str> = EXTENSION_CRATES
        .iter()
        .copied()
        .filter(|extension| !seams.contains(extension))
        .collect();
    let source: String = implementations
        .iter()
        .map(|extension| format!("use {} as _;\n", extension.replace('-', "_")))
        .collect();
    let root = write_probe_workspace(
        "source",
        &[
            ("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n"),
            ("bitty-runtime", "[package]\nname = \"bitty-runtime\"\n"),
        ],
        &[("crates/bitty-runtime/src/lib.rs", &source)],
    );
    let violations = extension_source_violations(&root);
    let _ = std::fs::remove_dir_all(&root);
    for extension in implementations {
        let ident = extension.replace('-', "_");
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(&format!("`{ident}`"))),
            "injected source identifier `{ident}` must be rejected: {violations:#?}"
        );
    }
}

#[test]
fn a11y_source_naming_is_restricted_to_host_seam() {
    // CTX-1362 / #2257: `bitty-a11y` is a seam for the dependency edge, but a
    // model seam must not become a general-purpose identifier. The host file
    // is legal; any other Core source naming it is a violation.
    let root = write_probe_workspace(
        "a11y-source",
        &[
            ("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n"),
            ("bitty-ui", "[package]\nname = \"bitty-ui\"\n"),
        ],
        &[
            (
                "crates/bitty-ui/src/a11y_host.rs",
                "use bitty_a11y::SnapshotHost;\nuse bitty_a11y_component::component::Message;\n",
            ),
            (
                "crates/bitty-ui/src/other.rs",
                "use bitty_a11y::Role;\nuse bitty_a11y_component::component::Message;\n",
            ),
        ],
    );
    let violations = extension_source_violations(&root);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(violations.len(), 2, "both model and alias names are fenced");
    assert!(
        violations
            .iter()
            .all(|violation| violation.contains("other.rs"))
    );
}

#[test]
fn execution_source_naming_is_restricted_to_runtime_seam() {
    // CTX-1403 / #2249, extended by #2494: `bitty-execution` is an
    // implementation mechanic, not a general-purpose contract Core may name
    // anywhere. The `bitty-runtime` bridge and host-client files are legal;
    // any other Core source naming it is a violation.
    let root = write_probe_workspace(
        "execution-source",
        &[
            ("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n"),
            ("bitty-runtime", "[package]\nname = \"bitty-runtime\"\n"),
        ],
        &[
            (
                "crates/bitty-runtime/src/execution_jobs.rs",
                "use bitty_execution::JobRegistry;\n",
            ),
            (
                "crates/bitty-runtime/src/execution_component/host.rs",
                "use bitty_execution::component::protocol::Message;\n",
            ),
            (
                "crates/bitty-runtime/src/other.rs",
                "use bitty_execution::JobId;\n",
            ),
        ],
    );
    let violations = extension_source_violations(&root);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        violations.len(),
        1,
        "only the runtime seam may name the execution supervisor: {violations:#?}"
    );
    assert!(
        violations[0].contains("other.rs") && violations[0].contains("bitty_execution"),
        "the non-seam source must be the offender: {violations:#?}"
    );
}

#[test]
fn graphics_seam_is_the_only_crate_name_reference() {
    // #2163 seam/fence part: the graphics crate name may appear only in the
    // designated seam module. This catches prose references (`bitty-graphics`)
    // that the identifier scan cannot see, so the boundary is not re-scattered
    // across module docs.
    let violations =
        crate_name_reference_violations(&workspace_root(), "bitty-graphics", GRAPHICS_SOURCE_SEAM);
    assert!(
        violations.is_empty(),
        "only `{GRAPHICS_SOURCE_SEAM}` may name the graphics extension crate: {violations:#?}"
    );
}

#[test]
fn crate_name_in_string_literals_is_not_source_coupling() {
    // #2381: a user-facing CLI example or diagnostic may spell the repository
    // name (`bitty extension install bitty-graphics`) without coupling Core to
    // the crate. Literal bodies are data; identifiers, paths, and prose are not.
    let root = write_probe_workspace(
        "crate-name-literals",
        &[("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n")],
        &[
            (
                "crates/bitty-terminal/src/hint.rs",
                "pub const HINT: &str = \"bitty extension install bitty-graphics\";\n",
            ),
            (
                "crates/bitty-terminal/src/raw.rs",
                "pub const RAW: &str = r#\"use bitty-graphics as g;\"#;\n",
            ),
            (
                "crates/bitty-terminal/src/encoded.rs",
                "pub const ESC: &str = \"a \\\" bitty-graphics \\\" b\";\npub const BYTES: &[u8] = b\"bitty-graphics\";\n",
            ),
        ],
    );
    let violations = crate_name_reference_violations(&root, "bitty-graphics", GRAPHICS_SOURCE_SEAM);
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        violations.is_empty(),
        "string literal text is user-facing data, not a source reference: {violations:#?}"
    );
}

#[test]
fn crate_name_outside_literals_still_fails_the_fence() {
    // Mutation guard for the fence: comments stay visible (module-doc prose is
    // the reason this scan exists) and a bare spelling outside a literal still
    // fails, while a literal-only mention in the same workspace does not.
    let root = write_probe_workspace(
        "crate-name-prose",
        &[("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n")],
        &[
            (
                "crates/bitty-terminal/src/module_docs.rs",
                "//! the bitty-graphics extension implements these traits\n",
            ),
            (
                "crates/bitty-terminal/src/line_comment.rs",
                "// bitty-graphics seam\npub fn f() {}\n",
            ),
            (
                "crates/bitty-terminal/src/literal_only.rs",
                "pub const HINT: &str = \"bitty-graphics\";\n",
            ),
        ],
    );
    let violations = crate_name_reference_violations(&root, "bitty-graphics", GRAPHICS_SOURCE_SEAM);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        violations.len(),
        2,
        "doc and line comments must stay fenced: {violations:#?}"
    );
    assert!(
        violations.iter().any(|v| v.contains("module_docs.rs")),
        "{violations:#?}"
    );
    assert!(
        violations.iter().any(|v| v.contains("line_comment.rs")),
        "{violations:#?}"
    );
    assert!(
        !violations.iter().any(|v| v.contains("literal_only.rs")),
        "{violations:#?}"
    );
}

#[test]
fn literal_stripping_preserves_code_lifetimes_and_comments() {
    // Helper mutation guard: a lifetime misread as a char literal would drop
    // the rest of the file and blind the fence, and an unterminated literal
    // must stay visible instead of swallowing a real reference.
    let source = "fn f<'a>(s: &'a str) -> &'a str { let _ = r#\"bitty-graphics\"#; let _ = b'x'; s }\n// bitty-graphics\n";
    let stripped = strip_literal_bodies(source);
    assert!(
        stripped.contains("fn f<'a>(s: &'a str) -> &'a str {"),
        "code around literals must survive: {stripped:?}"
    );
    assert!(
        !stripped.contains("'x'") && stripped.contains("s }"),
        "byte char literals must strip without eating following code: {stripped:?}"
    );
    assert!(
        !stripped.contains("bitty-graphics\"#"),
        "raw literal bodies must strip: {stripped:?}"
    );
    assert!(
        names_crate_spelling(&stripped, "bitty-graphics"),
        "comment mentions must stay visible: {stripped:?}"
    );
    assert!(
        !names_crate_spelling(
            &strip_literal_bodies("pub const H: &str = \"bitty-graphics\";"),
            "bitty-graphics"
        ),
        "literal-only text must not count as a reference"
    );
    assert!(
        names_crate_spelling(
            &strip_literal_bodies("pub const H: &str = \"oops bitty-graphics\n"),
            "bitty-graphics"
        ),
        "an unterminated literal must not hide the rest of the file"
    );
    assert!(
        names_crate_spelling(
            &strip_literal_bodies("fn f() { let _ = '\\\\'; }\n// bitty-graphics\n"),
            "bitty-graphics"
        ),
        "an escaped-backslash char literal must not swallow the next comment"
    );
}

/// Code-line (`//` comments stripped, whitespace compacted) references to a
/// Code-line (`//` comments stripped) references to the `kitty_seam` module.
/// Catches `pub use`, plain `use`, type-alias paths, and module aliases
/// (`pub use crate::kitty_seam as seam;`), with or without `crate::`/`self::`
/// qualification. The only allowed code lines are the module declarations.
fn seam_module_references(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let code = line.split("//").next().unwrap_or("").trim();
            let compact: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            if compact.is_empty()
                || matches!(compact.as_str(), "pubmodkitty_seam;" | "modkitty_seam;")
            {
                return None;
            }
            // Tokenize the original (whitespace keeps `use kitty_seam` from
            // merging into one identifier); `crate::`/`self::`/`::` boundaries
            // and aliases all leave `kitty_seam` as its own token.
            let names_module = code
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|token| token == "kitty_seam");
            names_module.then(|| code.to_string())
        })
        .collect()
}

#[test]
fn graphics_seam_is_the_only_public_seam_path() {
    // #2163 seam/fence part: the decode/raster traits and backend are reached
    // through `bitty_rich::kitty_seam` only. A crate-root re-export, use, type
    // alias, or module alias would give call sites a second, un-fenced path to
    // the extension boundary.
    let root = workspace_root();
    let lib = std::fs::read_to_string(root.join("crates/bitty-rich/src/lib.rs"))
        .expect("read bitty-rich lib.rs");
    assert!(
        lib.contains("pub mod kitty_seam;"),
        "the kitty_seam module must stay public"
    );
    let references = seam_module_references(&lib);
    assert!(
        references.is_empty(),
        "crate-root seam references must go through `bitty_rich::kitty_seam` at call sites: {references:?}"
    );
}

#[test]
fn graphics_seam_module_bypass_is_rejected() {
    // #2163 guard mutation: the old literal prefix check missed
    // `pub use crate::kitty_seam::... as Alias;`, type aliases, and
    // `pub use crate::kitty_seam as seam;`. Every path/module form is now
    // rejected; the module declarations and comments are not.
    for probe in [
        "pub use kitty_seam::CoreKittyBackend as SeamBackend;",
        "pub use crate::kitty_seam::CoreKittyBackend as SeamBackend;",
        "pub use self::kitty_seam::KittyDecodeBackend;",
        "use self :: kitty_seam :: KittyTransmitFormat;",
        "pub type SeamBackend = crate::kitty_seam::CoreKittyBackend;",
        "pub use crate::kitty_seam as seam;",
        "use crate::{kitty_seam as seam};",
    ] {
        assert!(
            !seam_module_references(probe).is_empty(),
            "seam module bypass must be rejected: {probe}"
        );
    }
    for allowed in [
        "pub mod kitty_seam;",
        "mod kitty_seam;",
        "// pub use kitty_seam::CoreKittyBackend;",
        "//! [`kitty_seam`] traits",
    ] {
        assert!(
            seam_module_references(allowed).is_empty(),
            "module declaration/comment must stay allowed: {allowed}"
        );
    }
}

#[test]
fn graphics_source_naming_is_restricted_to_kitty_seam() {
    // #2163 mutation proof: a direct import or crate-name mention is reported
    // everywhere except the designated graphics seam module.
    let root = write_probe_workspace(
        "graphics-source",
        &[
            ("bitty-terminal", "[package]\nname = \"bitty-terminal\"\n"),
            ("bitty-rich", "[package]\nname = \"bitty-rich\"\n"),
            ("bitty-runtime", "[package]\nname = \"bitty-runtime\"\n"),
        ],
        &[
            (
                GRAPHICS_SOURCE_SEAM,
                "use bitty_graphics::KittyDecodeBackend;\n// bitty-graphics seam\n",
            ),
            (
                "crates/bitty-runtime/src/runtime.rs",
                "use bitty_graphics::KittyDecodeBackend;\n// bitty-graphics elsewhere\n",
            ),
        ],
    );
    let identifier_violations = extension_source_violations(&root);
    let crate_name_violations =
        crate_name_reference_violations(&root, "bitty-graphics", GRAPHICS_SOURCE_SEAM);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        identifier_violations.len(),
        1,
        "only the non-seam source may name the identifier: {identifier_violations:#?}"
    );
    assert!(
        identifier_violations[0].contains("runtime.rs")
            && identifier_violations[0].contains("bitty_graphics"),
        "the non-seam source must be the offender: {identifier_violations:#?}"
    );
    assert_eq!(
        crate_name_violations.len(),
        1,
        "only the non-seam source may mention the crate name: {crate_name_violations:#?}"
    );
    assert!(
        crate_name_violations[0].contains("runtime.rs"),
        "the non-seam source must be the crate-name offender: {crate_name_violations:#?}"
    );
}

/// Collect `*.rs` files under `dir` (non-recursive walk, skips `target/`).
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

/// Source-level fence violations: Core library sources may not name an
/// implementation crate. Seam crates are legal by design, except a source
/// seam restricted by [`SOURCE_SEAM_FILES`]; `bitty-storage` is legal only in
/// its composition-root adapter, default-off host, and hermetic fixture. The
/// graphics entry is a source seam with no dependency edge, so it is legal only
/// in [`GRAPHICS_SOURCE_SEAM`].
fn extension_source_violations(root: &Path) -> Vec<String> {
    let members = workspace_members(root);
    let mut violations = Vec::new();
    for extension in EXTENSION_CRATES {
        let source_seam = SOURCE_SEAM_FILES
            .iter()
            .find(|(seam, _)| seam == extension)
            .map(|(_, paths)| paths.iter().map(|path| root.join(path)).collect::<Vec<_>>());
        if source_seam.is_none() && SEAM_CRATES.iter().any(|(seam, _)| seam == extension) {
            continue;
        }
        let identifiers: Vec<String> = SOURCE_SEAM_IDENTIFIERS
            .iter()
            .find(|(seam, _)| *seam == *extension)
            .map_or_else(
                || vec![extension.replace('-', "_")],
                |(_, ids)| ids.iter().map(|id| (*id).to_owned()).collect(),
            );
        for member in &members {
            let src = root.join("crates").join(member).join("src");
            if !src.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            rust_files(&src, &mut files);
            for file in files {
                // A source-restricted seam (storage, a11y, execution, or
                // graphics) is the only place allowed to name its own
                // implementation; every other identifier is banned everywhere.
                if source_seam
                    .as_ref()
                    .is_some_and(|paths| paths.contains(&file))
                {
                    continue;
                }
                let text = std::fs::read_to_string(&file).expect("read rs file");
                for ident in &identifiers {
                    if names_crate(&text, ident) {
                        violations.push(format!("{} names `{ident}`", file.display()));
                    }
                }
            }
        }
    }
    violations
}

/// Source-reference violations for the hyphenated crate spelling, which
/// [`names_crate`] cannot see. Only `seam` may name the extension this way.
///
/// String and char literal bodies are exempt ([`strip_literal_bodies`]): a CLI
/// example or diagnostic that prints the repository-style name is user-facing
/// data, not a source reference. Comments and code stay fenced, so prose in
/// module docs and any bare spelling still fail.
fn crate_name_reference_violations(root: &Path, krate: &str, seam: &str) -> Vec<String> {
    let members = workspace_members(root);
    let seam = root.join(seam);
    let mut violations = Vec::new();
    for member in &members {
        let src = root.join("crates").join(member).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            if file == seam {
                continue;
            }
            let text = std::fs::read_to_string(&file).expect("read rs file");
            if names_crate_spelling(&strip_literal_bodies(&text), krate) {
                violations.push(format!("{} names `{krate}`", file.display()));
            }
        }
    }
    violations
}

#[test]
fn core_library_sources_never_name_extension_impls() {
    // Core-never-imports-extension at the source level (CTX-1086 / #1890,
    // generalizing W-146 DEC-W146-2 and extended by CTX-1364 / #2256): no Core
    // library source may name an implementation. The manifest-level gate stays
    // green when Core duplicates extension logic without a `Cargo.toml` edge
    // (audit CTX-1085 F10), so this scan rejects the Rust identifiers directly.
    // The composition-root exception is `storage_backends.rs`, which may name
    // `bitty_storage` to implement the Core-owned backends behind Core
    // validation; the default-off component host and its hermetic fixture are
    // the only other storage naming points (CTX-1479 / bitty#2495). Every
    // other crate reaches persistence through the
    // `SessionFileBackend` / `KvCommitBackend` traits. Seam crates
    // (`bitty-ipc`, `bitty-network-wire`, `bitty-network-publication`,
    // `bitty-observability-api`, `bitty-platform-services`,
    // `bitty-url-detector`) are linked contracts Core may name by design. The
    // a11y model seam is source-restricted: only
    // `crates/bitty-ui/src/a11y_host.rs` may name `bitty_a11y`
    // (`SOURCE_SEAM_FILES`, CTX-1362 / #2257). The execution mechanic seam is
    // likewise restricted to `crates/bitty-runtime/src/execution_jobs.rs`
    // (`SOURCE_SEAM_FILES`, CTX-1403 / #2249). The graphics seam has no
    // production edge yet, but its identifier is restricted to
    // `crates/bitty-rich/src/kitty_seam.rs` (`SOURCE_SEAM_FILES`, #2163).
    let violations = extension_source_violations(&workspace_root());
    assert!(
        violations.is_empty(),
        "Core library sources must not name extension implementations: {violations:#?}"
    );
}

#[test]
fn safe_policy_selects_zero_third_party() {
    // `--safe` starts with zero third-party plugins: the safe load policy
    // drops every third-party candidate before any VM is built (the same
    // `LoadPolicy::safe_mode` the composition root wires to `args.safe`).
    use bitty_lua::gate::{
        LoadPolicy, PluginCandidate, count_third_party_selected, select_candidates,
    };

    let policy = LoadPolicy::safe_mode();
    assert!(policy.is_safe_mode());
    assert!(!policy.allows_third_party());
    let candidates = vec![
        PluginCandidate::first_party("core.example"),
        PluginCandidate::third_party("evil-exec.example"),
        PluginCandidate::third_party("evil-io.example"),
        PluginCandidate::third_party("evil-require.example"),
    ];
    let selected = select_candidates(&policy, &candidates);
    assert_eq!(
        selected.len(),
        1,
        "safe mode admits only first-party: {selected:?}"
    );
    assert_eq!(count_third_party_selected(&policy, &candidates), 0);
}

/// Writes one dev-root package: `<root>/<id>/bitty-plugin.toml` plus
/// `<root>/<id>/lua/init.lua` (dev roots resolve as third-party `local-path`
/// provenance, so `--safe` must skip them with no VM).
fn write_dev_plugin(root: &Path, id: &str) {
    let plugin = root.join(id);
    std::fs::create_dir_all(plugin.join("lua")).expect("plugin dirs");
    std::fs::write(
        plugin.join("bitty-plugin.toml"),
        format!(
            "[plugin]\nid = \"{id}\"\nname = \"Hostile Dev Plugin\"\nversion = \"0.0.1\"\n\
             description = \"safe-mode skip probe\"\n\n\
             [compat]\nbitty = \">=0.0.1\"\nplugin-api = \"^1.0\"\n\n\
             [capabilities]\nui.overlay.focus = true\n\n\
             [lazy]\ncommands = []\nevents = []\n",
        ),
    )
    .expect("write manifest");
    std::fs::write(
        plugin.join("lua/init.lua"),
        "os.execute('touch pwned-by-safe-test')\nreturn {}\n",
    )
    .expect("write init.lua");
}

#[test]
fn safe_headless_startup_skips_hostile_dev_plugin_with_no_vm() {
    // End-to-end `--safe` startup trace: with a hostile third-party plugin on
    // `BITTY_PLUGIN_DIR`, safe startup exits 0, reports the skip, and never
    // activates a plugin VM (no `active (` line, no executed payload).
    let tag = format!(
        "w147-safe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    );
    let root = std::env::temp_dir().join(tag);
    std::fs::create_dir_all(&root).expect("scratch dir");
    let plugin_dir = root.join("plugins");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    write_dev_plugin(&plugin_dir, "evil-safe-probe.example");

    let output = std::process::Command::new(BITTY_BIN)
        .args(["--safe", "--headless", "--log-level", "info"])
        .current_dir(&root)
        .env("XDG_CONFIG_HOME", &root)
        .env("XDG_DATA_HOME", &root)
        .env("HOME", &root)
        .env("BITTY_PLUGIN_DIR", &plugin_dir)
        .env("NO_COLOR", "1")
        .env_remove("BITTY_CONFIG")
        .env_remove("BITTY_PROFILE")
        .output()
        .unwrap_or_else(|err| panic!("spawn {BITTY_BIN:?}: {err}"));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let payload_ran =
        plugin_dir.join("pwned-by-safe-test").exists() || root.join("pwned-by-safe-test").exists();
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(
        output.status.code(),
        Some(0),
        "safe headless startup must exit 0, stderr={stderr:?}"
    );
    assert!(
        stderr.contains("skipped (--safe"),
        "safe startup must log the plugin skip trace, stderr={stderr:?}"
    );
    assert!(
        !stderr.contains(" active ("),
        "safe startup must activate no plugin VM, stderr={stderr:?}"
    );
    assert!(!payload_ran, "hostile plugin payload must never execute");
}
