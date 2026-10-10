//! W-147 final verification gate: small-core dependency graph + safe startup
//! (CTX-0940, issue #1621).
//!
//! These tests pin the program end-state so a later change cannot silently
//! re-grow the retired surface:
//!
//! - validation suites (`bitty-compat-lab`, `bitty-perf`) live outside the
//!   workspace (W-105, CTX-0931): no workspace member, no normal dependency
//!   edge;
//! - harness-only crates (`bitty-test-support`, `bitty-test-vm`) are
//!   dev-dependencies only: no production (`[dependencies]`) edge anywhere;
//! - extension mechanics (`bitty-storage`, `bitty-graphics`, `bitty-execution`,
//!   `bitty-a11y`) link only through the composition root (`bitty-terminal`),
//!   behind Core-owned traits; Core library sources never name an extension
//!   implementation (`Core-never-imports-extension`, W-146 DEC-W146-2,
//!   generalized to all four by CTX-1086 / #1890);
//! - `bitty --safe` starts with zero third-party plugins: the safe load
//!   policy selects nothing hostile, and the built binary skips a hostile
//!   dev-root plugin with no VM.
//!
//! Source-level vs manifest-level: the `Cargo.toml` gate
//! (`only_composition_root_links_storage_extension`) proves no production
//! dependency edge exists, but it stays green when Core duplicates extension
//! logic without declaring a dependency (audit CTX-1085 F10: kitty
//! decode/raster, process supervisors, a11y model). The
//! `core_library_sources_never_name_extension_impls` scan closes that gap by
//! rejecting the `bitty_storage` / `bitty_execution` / `bitty_graphics` /
//! `bitty_a11y` identifiers in Core library sources. To fence a fifth
//! extension crate, add one entry to `EXTENSION_CRATES` (storage stays first).
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

/// Extension crates that must link only where the seam requires (W-140..W-146).
/// `bitty-storage` is the single permitted production edge, owned by the
/// composition root; execution/graphics/a11y must not appear at all.
const EXTENSION_CRATES: &[&str] = &[
    "bitty-storage",
    "bitty-execution",
    "bitty-graphics",
    "bitty-a11y",
];

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

/// Classifies a section header: `Some(None)` for a normal dependency list
/// section, `Some(Some(name))` for a `[dependencies.<name>]`-style table
/// header (the edge is the table name), `None` for anything else
/// (dev/build sections, `[package]`, `[features]`, ...).
fn normal_dep_section(header: &str) -> Option<Option<String>> {
    let inner = header.strip_prefix('[')?.strip_suffix(']')?;
    let parts: Vec<String> = inner
        .split('.')
        .map(|part| {
            part.trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string()
        })
        .collect();
    match parts
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["dependencies"] => Some(None),
        ["dependencies", name] => Some(Some((*name).to_string())),
        ["target", .., "dependencies"] => Some(None),
        ["target", .., "dependencies", name] => Some(Some((*name).to_string())),
        _ => None,
    }
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
fn only_composition_root_links_storage_extension() {
    // W-146 DEC-W146-2: the composition root owns the extracted storage
    // mechanics; Core library crates consume persistence through Core-owned
    // traits. Execution/graphics/a11y have no production edge at all.
    let root = workspace_root();
    let members = workspace_members(&root);
    let (linked, unlinked) = EXTENSION_CRATES
        .split_first()
        .expect("EXTENSION_CRATES names the linked seam first");
    assert_eq!(
        *linked, "bitty-storage",
        "test contract: EXTENSION_CRATES[0] is the composition-root seam"
    );
    let storage_consumers = normal_consumers_of(&root, &members, linked);
    assert_eq!(
        storage_consumers,
        vec!["bitty-terminal".to_string()],
        "only the composition root may link `bitty-storage`, got: {storage_consumers:?}"
    );
    for dep in unlinked {
        let consumers = normal_consumers_of(&root, &members, dep);
        assert!(
            consumers.is_empty(),
            "extension crate `{dep}` must have no `[dependencies]` edge, found in: {consumers:?}"
        );
    }
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

#[test]
fn core_library_sources_never_name_extension_impls() {
    // Core-never-imports-extension at the source level (CTX-1086 / #1890,
    // generalizing W-146 DEC-W146-2): no Core library source may name an
    // extension implementation. The manifest-level gate stays green when Core
    // duplicates extension logic without a `Cargo.toml` edge (audit CTX-1085
    // F10), so this scan rejects the Rust identifiers directly. The only
    // exception is the composition-root seam (`storage_backends.rs`), which
    // may name `bitty_storage` to implement the Core-owned backends behind
    // Core validation; every other crate reaches persistence through the
    // `SessionFileBackend` / `KvCommitBackend` traits. `EXTENSION_CRATES[0]`
    // stays the storage seam so the storage case runs first; to fence a fifth
    // extension crate, add one entry to `EXTENSION_CRATES`.
    let root = workspace_root();
    let members = workspace_members(&root);
    let (first, _) = EXTENSION_CRATES
        .split_first()
        .expect("EXTENSION_CRATES names the linked seam first");
    assert_eq!(
        *first, "bitty-storage",
        "test contract: EXTENSION_CRATES[0] is the composition-root seam"
    );
    let seam = root
        .join("crates")
        .join("bitty-terminal")
        .join("src")
        .join("storage_backends.rs");
    for dep in EXTENSION_CRATES {
        let ident = dep.replace('-', "_");
        let mut offenders = Vec::new();
        for member in &members {
            let src = root.join("crates").join(member).join("src");
            if !src.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            rust_files(&src, &mut files);
            for file in files {
                // Only the storage seam may name its implementation; the
                // graphics/execution/a11y identifiers are banned everywhere,
                // including the seam.
                if *dep == "bitty-storage" && file == seam {
                    continue;
                }
                let text = std::fs::read_to_string(&file).expect("read rs file");
                if names_crate(&text, &ident) {
                    offenders.push(file);
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "Core library sources must not name the {ident} implementation: {offenders:?}"
        );
    }
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
