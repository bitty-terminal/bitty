//! Retired rich-presentation helper guard for bitty#2117 (core-split).
//!
//! The accepted graphics contract keeps the trust decision and the scene
//! composition in Core (`W-133` / ADR 0016 Boundary 2): bounded intake,
//! `ImageStore`/`ImagePlacement`, the aggregate budget, pre-upload
//! validation, and the rich scene model are Core-retained implementation,
//! not extraction material. Only decode/raster are extension-bound.
//!
//! CTX-1412 retired two *unwired helpers* that no Core path consumed while
//! the scene/image/place/background surface stays Core-retained:
//!
//! - `src/presentation.rs` - the `RichPresentation` snapshot aggregation
//!   view (hyperlinks + shell zones + clipboard + command blocks + the
//!   legacy Kitty placeholder stub), self-referential to its own tests;
//! - `src/projection.rs` - the fragment-to-`RichBlock` projection, whose
//!   only purpose was to bridge `bitty-ipc` ingestion DTOs.
//!
//! The second one mattered beyond dead code: it was the sole reason
//! `bitty-rich` carried a production dependency edge to the extracted
//! `bitty-ipc` repository (ADR 0015 partitions IPC out of Core; the W-147
//! fence covers storage/graphics/execution/a11y but not IPC, so a
//! manifest-level edge inside a Core crate could re-grow silently).
//!
//! This guard closes both blind spots directly. Mutation proof (recorded in
//! the PR): restoring either module file, naming a retired symbol from any
//! `crates/*/src` file, re-declaring the module in `lib.rs`, or re-adding a
//! `bitty-ipc` dependency to `crates/bitty-rich/Cargo.toml` fails a test
//! here; removing the offending change passes again.
//!
//! CTX-1415 (bitty#2358 / bitty#2359) retired the two remaining unwired
//! legacy surfaces:
//!
//! - `src/kitty.rs` - the streaming Kitty placeholder stub. Real bounded
//!   `APC G` intake lives in `bitty-vt/src/kitty_apc.rs`, and decode /
//!   placement / ledger policy lives in `src/kitty_place.rs`; the two
//!   surviving bounds moved there with values unchanged
//!   (`KITTY_PLACE_MAX_IMAGES` = 64 via `IMAGE_STORE_MAX_ENTRIES`,
//!   `KITTY_LEDGER_MAX_BYTES` = 320,000,000).
//! - `src/wallpaper_theme.rs` - the wallpaper-derived colorscheme
//!   derivation. Theme derivation is presentation policy owned by plugins
//!   (the ADR 0014 Core-mechanism/plugin-presentation split): the wired path
//!   is the plugin `bitty.image.read` bridge and preview/confirm intents
//!   (CTX-1169 / bitty#1976), not a Core module. bitty#2359 records the
//!   retire decision and its evidence.
//!
//! Scope note: the retained scene/image/place/background modules and the
//! decode/raster seam copies are governed by the `W-133` retention decision
//! and their own tracked follow-ups, not by this retirement.

use std::path::{Path, PathBuf};

/// Module files retired from Core.
const RETIRED_MODULE_FILES: &[&str] = &[
    "crates/bitty-rich/src/presentation.rs",
    "crates/bitty-rich/src/projection.rs",
    "crates/bitty-rich/src/kitty.rs",
    "crates/bitty-rich/src/wallpaper_theme.rs",
];

/// Lowercase module names that must not be re-declared in `lib.rs`.
const RETIRED_MODULE_NAMES: &[&str] = &["presentation", "projection", "kitty", "wallpaper_theme"];

/// Identifiers exported only by the retired modules. Generic words
/// (`projection`, `presentation`, `luminance`, `contrast_ratio`) are
/// deliberately absent: they appear in unrelated prose and UI types.
const RETIRED_IDENTIFIERS: &[&str] = &[
    "RichPresentation",
    "ProjectedBlock",
    "ProjectionError",
    "project_fragments",
    "KittyGraphicsStub",
    "KittyPlaceholder",
    "KittyPlaceholderId",
    "KittyChunkOutcome",
    "KittyChunkError",
    "KITTY_MAX_PLACEHOLDERS",
    "KITTY_MAX_PAYLOAD_BYTES",
    "KITTY_MAX_CHUNKED_BYTES",
    "WallpaperPalette",
    "WallpaperThemePreview",
    "WallpaperThemeError",
    "WALLPAPER_THEME_ANSI_COUNT",
    "derive_palette",
    "derive_palette_from_bytes",
    "preview_from_bytes",
    "preview_from_image",
    "planned_sample_count",
    "sample_stride",
];

/// Workspace root derived from this crate's manifest dir (no hardcoded
/// checkout paths): `<workspace>/crates/bitty-rich` -> `<workspace>`.
fn workspace_root() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .expect("crates/<name> parent layout")
        .to_path_buf()
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

/// Every `crates/<member>/src` tree in the workspace (the source-level fence
/// covers Core libraries, exactly like the W-147 scan; `tests/` is not
/// scanned so this guard's own constants cannot match themselves).
fn core_src_roots(root: &Path) -> Vec<PathBuf> {
    let crates_dir = root.join("crates");
    let entries =
        std::fs::read_dir(&crates_dir).unwrap_or_else(|err| panic!("read crates dir: {err}"));
    let mut roots = Vec::new();
    for entry in entries {
        let src = entry.expect("crate entry").path().join("src");
        if src.is_dir() {
            roots.push(src);
        }
    }
    roots
}

/// True when Rust source names `ident` as a token (word-boundary match), so
/// `RichPresentation::new` and `use RichPresentation as View;` both match
/// while `RichPresentation2` does not.
fn names_identifier(text: &str, ident: &str) -> bool {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|token| token == ident)
}

/// True when the text declares a module (`mod <name>;` / `pub mod <name>;`).
fn declares_module(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        let code = line.split("//").next().unwrap_or("").trim();
        let rest = code.strip_prefix("pub ").unwrap_or(code);
        let Some(rest) = rest.strip_prefix("mod ") else {
            return false;
        };
        rest.trim()
            .strip_suffix(';')
            .is_some_and(|decl| decl.trim() == name)
    })
}

/// Lines of the dependency sections of a manifest (normal and dev/build
/// families), with comments stripped so prose about a crate never reads as
/// an edge. Returns `(section, code_line)` pairs.
fn dependency_lines(manifest: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut collecting = false;
    let mut lines = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            collecting = dep_section_kind(&header).is_some();
            if let Some(DepSection::Table(name)) = dep_section_kind(&header) {
                // Table form carries the edge in the header; the body never
                // names the dependency (except an optional `package` rename,
                // which stays collected below).
                lines.push((header.clone(), format!("{name} = {{ table header }}")));
            }
            section = header;
            continue;
        }
        if !collecting {
            continue;
        }
        let code = line.split('#').next().unwrap_or("").trim().to_string();
        if !code.is_empty() {
            lines.push((section.clone(), code));
        }
    }
    lines
}

/// A dependency section: either the plain list (`[dependencies]`, also under
/// `[target.<cfg>]`) or a `[dependencies.<name>]`-style table for one crate.
enum DepSection {
    List,
    Table(String),
}

/// Classifies a section header (`dependencies`, `dev-dependencies`,
/// `build-dependencies`, optionally prefixed by `target.<cfg>.`). Returns
/// `None` for `[package]`, `[features]`, and every other table.
fn dep_section_kind(header: &str) -> Option<DepSection> {
    let parts: Vec<&str> = header.split('.').map(str::trim).collect();
    let index = parts.iter().rposition(|part| {
        matches!(
            *part,
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    })?;
    match parts.len() - index {
        1 => Some(DepSection::List),
        2 => Some(DepSection::Table(
            parts[index + 1]
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string(),
        )),
        _ => None,
    }
}

/// True when a dependency line declares `dep` (hyphen or underscore spelling,
/// or a `package = "dep"` rename).
fn declares_dep(line: &str, dep: &str) -> bool {
    let spaceless: String = line.split_whitespace().collect();
    [dep, &dep.replace('-', "_")].iter().any(|name| {
        line.starts_with(&format!("{name} "))
            || line.starts_with(&format!("{name}="))
            || spaceless.contains(&format!("package=\"{name}\""))
            || spaceless.contains(&format!("package='{name}'"))
    })
}

#[test]
fn retired_module_files_stay_absent() {
    let root = workspace_root();
    for retired in RETIRED_MODULE_FILES {
        let path = root.join(retired);
        assert!(
            !path.exists(),
            "`{retired}` was retired with no Core consumer (CTX-1412 / CTX-1415, \
             bitty#2117 / bitty#2358 / bitty#2359). If the surface is needed again, \
             it belongs on the extension or plugin side of the seam, not back in Core."
        );
    }
}

#[test]
fn retired_modules_are_not_redeclared() {
    let root = workspace_root();
    let lib = root.join("crates/bitty-rich/src/lib.rs");
    let text = std::fs::read_to_string(&lib).expect("read bitty-rich lib.rs");
    for name in RETIRED_MODULE_NAMES {
        assert!(
            !declares_module(&text, name),
            "`bitty-rich` must not re-declare the retired `{name}` module \
             (CTX-1412 / CTX-1415, bitty#2117 / bitty#2358 / bitty#2359)"
        );
    }
}

#[test]
fn core_sources_never_name_retired_symbols() {
    let root = workspace_root();
    let mut offenders = Vec::new();
    for src in core_src_roots(&root) {
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        for file in files {
            let text = std::fs::read_to_string(&file).expect("read rs file");
            for ident in RETIRED_IDENTIFIERS {
                if names_identifier(&text, ident) {
                    offenders.push(format!("{}: {ident}", file.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "Core sources must not name retired rich-presentation symbols \
         (CTX-1412 / CTX-1415, bitty#2117 / bitty#2358 / bitty#2359): {offenders:?}"
    );
}

#[test]
fn dependency_parser_detects_edge_forms() {
    // Regression for the guard itself: table headers, renamed packages, and
    // comment-stripped prose must classify exactly like the W-147 parser.
    let manifest = r#"
[package]
name = "probe"

[dependencies]
bitty-ipc = { version = "0.0.1" }

[dev-dependencies]
ipc = { package = "bitty-ipc", version = "0.0.1" }

[dependencies.ipc-table]
package = "bitty-ipc"
version = "0.0.1"

[target.'cfg(unix)'.build-dependencies]
other = "1"
"#;
    let lines = dependency_lines(manifest);
    assert_eq!(
        lines.len(),
        6,
        "list entries plus the table header/body are collected: {lines:?}"
    );
    let ipc_edges: Vec<&String> = lines
        .iter()
        .filter(|(_, line)| declares_dep(line, "bitty-ipc"))
        .map(|(_, line)| line)
        .collect();
    assert_eq!(
        ipc_edges.len(),
        3,
        "plain, renamed-inline, and table-rename spellings must all read as edges: {lines:?}"
    );
    assert!(
        !declares_dep("other = \"1\"", "bitty-ipc"),
        "an unrelated dependency is not an IPC edge"
    );
    let commented = "[dependencies]\n# bitty-ipc = { version = \"0.0.1\" }\nother = \"1\"\n";
    assert!(
        !dependency_lines(commented)
            .iter()
            .any(|(_, line)| declares_dep(line, "bitty-ipc")),
        "a commented-out dependency is not an edge"
    );
}

#[test]
fn bitty_rich_keeps_no_ipc_production_edge() {
    // The retired projection module was the only consumer of `bitty-ipc` in
    // this crate. ADR 0015 partitions IPC out of Core, so an IPC edge must
    // not come back through `bitty-rich` for an unconsumed module; IPC
    // wiring belongs to the composition root or the extension side.
    let root = workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("crates/bitty-rich/Cargo.toml")).expect("read manifest");
    let offenders: Vec<String> = dependency_lines(&manifest)
        .into_iter()
        .filter(|(_, line)| declares_dep(line, "bitty-ipc"))
        .map(|(section, line)| format!("[{section}] {line}"))
        .collect();
    assert!(
        offenders.is_empty(),
        "`bitty-rich` must keep no `bitty-ipc` dependency edge (CTX-1412 / bitty#2117): {offenders:?}"
    );
}
