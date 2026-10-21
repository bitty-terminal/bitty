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
//!   `bitty-terminal` and named only in `storage_backends.rs`.
//!   `bitty-a11y` is the deliberate accessibility snapshot-model seam
//!   (CTX-1362 / #2257): linked only by `bitty-ui` and named only in
//!   `crates/bitty-ui/src/a11y_host.rs` (`SOURCE_SEAM_FILES`).
//!   `bitty-execution` is the Core-runtime execution mechanic seam
//!   (CTX-1403 / #2249): linked only by `bitty-runtime` and named only in
//!   `crates/bitty-runtime/src/execution_jobs.rs` (`SOURCE_SEAM_FILES`).
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
    // CTX-1362 / #2257: the accepted W-134 snapshot-model seam. It is a
    // model/contract crate (zero dependencies, `forbid(unsafe)`, no PTY, GPU,
    // window, or plugin handle) consumed by `bitty-ui`'s `SnapshotHost`
    // implementation. Unlike the other seams, source naming is restricted to
    // one file by `SOURCE_SEAM_FILES`.
    ("bitty-a11y", &["bitty-ui"]),
    // CTX-1403 / #2249: the Core-runtime execution mechanic seam. It is an
    // implementation crate, not a contract/codec seam, but it has the same
    // exact-consumer + single-source-file shape: `bitty-runtime` owns the
    // tool/plugin spawn surface and only `execution_jobs.rs` may name it.
    ("bitty-execution", &["bitty-runtime"]),
];

/// Seam crates whose Core source references are restricted to one file.
///
/// `bitty-a11y` is a model seam, not a general-purpose contract Core may name
/// anywhere: exactly `crates/bitty-ui/src/a11y_host.rs` implements
/// `SnapshotHost`, and every other source imports the role vocabulary through
/// that module's re-export. `bitty-execution` is likewise confined to the
/// `bitty-runtime` `execution_jobs.rs` bridge (#2249).
const SOURCE_SEAM_FILES: &[(&str, &str)] = &[
    ("bitty-a11y", "crates/bitty-ui/src/a11y_host.rs"),
    (
        "bitty-execution",
        "crates/bitty-runtime/src/execution_jobs.rs",
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
                "use bitty_a11y::SnapshotHost;\n",
            ),
            ("crates/bitty-ui/src/other.rs", "use bitty_a11y::Role;\n"),
        ],
    );
    let violations = extension_source_violations(&root);
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(
        violations.len(),
        1,
        "only the host seam may name the a11y model: {violations:#?}"
    );
    assert!(
        violations[0].contains("other.rs") && violations[0].contains("bitty_a11y"),
        "the non-seam source must be the offender: {violations:#?}"
    );
}

#[test]
fn execution_source_naming_is_restricted_to_runtime_seam() {
    // CTX-1403 / #2249: `bitty-execution` is an implementation mechanic, not
    // a general-purpose contract Core may name anywhere. The
    // `bitty-runtime` bridge file is legal; any other Core source naming it
    // is a violation.
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
/// its composition-root seam file.
fn extension_source_violations(root: &Path) -> Vec<String> {
    let members = workspace_members(root);
    let storage_seam = root
        .join("crates")
        .join("bitty-terminal")
        .join("src")
        .join("storage_backends.rs");
    let mut violations = Vec::new();
    for extension in EXTENSION_CRATES {
        let is_storage = *extension == "bitty-storage";
        let source_seam = SOURCE_SEAM_FILES
            .iter()
            .find(|(seam, _)| seam == extension)
            .map(|(_, path)| root.join(path));
        if !is_storage
            && source_seam.is_none()
            && SEAM_CRATES.iter().any(|(seam, _)| seam == extension)
        {
            continue;
        }
        let ident = extension.replace('-', "_");
        for member in &members {
            let src = root.join("crates").join(member).join("src");
            if !src.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            rust_files(&src, &mut files);
            for file in files {
                // The storage composition-root seam and a source-restricted
                // model seam are the only files that may name their
                // implementation; every other identifier is banned everywhere.
                if is_storage && file == storage_seam {
                    continue;
                }
                if source_seam.as_ref() == Some(&file) {
                    continue;
                }
                let text = std::fs::read_to_string(&file).expect("read rs file");
                if names_crate(&text, &ident) {
                    violations.push(format!("{} names `{ident}`", file.display()));
                }
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
    // validation; every other crate reaches persistence through the
    // `SessionFileBackend` / `KvCommitBackend` traits. Seam crates
    // (`bitty-ipc`, `bitty-network-wire`, `bitty-network-publication`,
    // `bitty-observability-api`, `bitty-platform-services`,
    // `bitty-url-detector`) are linked contracts Core may name by design. The
    // a11y model seam is source-restricted: only
    // `crates/bitty-ui/src/a11y_host.rs` may name `bitty_a11y`
    // (`SOURCE_SEAM_FILES`, CTX-1362 / #2257). The execution mechanic seam is
    // likewise restricted to `crates/bitty-runtime/src/execution_jobs.rs`
    // (`SOURCE_SEAM_FILES`, CTX-1403 / #2249).
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
