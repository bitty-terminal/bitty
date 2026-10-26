//! Bounded system font discovery for `bitty list fonts` / `bitty +list-fonts`
//! (CTX-1467, issue #2464).
//!
//! The catalog reuses the process-wide font-index database the shaped
//! rasterizer already owns ([`crate::shaped`]) instead of introducing a
//! second discovery mechanism: the same cached face metadata (platform font
//! directories plus fontconfig XML on Linux) the renderer resolves. Nothing
//! here opens a network connection, spawns a process, or loads a plugin VM;
//! face bytes are not read for the catalog.
//!
//! # Contract
//!
//! - Rows are unique `(family, style, source)` tuples, sorted by family then
//!   style, capped at [`MAX_CATALOG_FACES`].
//! - `total` is the unique count before the cap; `faces.len() <= total` and
//!   `truncated == total > faces.len()`.
//! - Family/style strings are scrubbed of control characters and capped at
//!   [`MAX_CATALOG_TEXT_BYTES`] bytes on a character boundary, so hostile font
//!   metadata can never corrupt a table or JSON envelope.
//! - `source` is `user` (a face under one of the caller-provided user font
//!   roots), `system` (any other file face), or `embedded` (in-memory face).
//!   Absolute paths never travel in the catalog.
//! - Unsupported targets return an empty catalog with `supported == false`.
//!
//! Discovery is read-only host state. A missing fontconfig installation or a
//! headless session yields an empty or partial catalog, never a panic: the
//! `fontdb` scan is safe and this module performs no unwraps on host input.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fontdb::{Source, Style as DbStyle, Weight};

/// Maximum rows one catalog call may return.
pub const MAX_CATALOG_FACES: usize = 512;

/// Maximum bytes for a family or style label.
pub const MAX_CATALOG_TEXT_BYTES: usize = 128;

/// One unique face row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CatalogFace {
    /// Primary family name (English US when the face carries one).
    pub family: String,
    /// Owned style label (`Regular`, `Bold Italic`, `350 Oblique`, ...).
    pub style: String,
    /// Coarse origin: `user`, `system`, or `embedded`.
    pub source: String,
}

/// Bounded result of one discovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceCatalog {
    /// Sorted, capped rows.
    pub faces: Vec<CatalogFace>,
    /// Unique face count before the cap.
    pub total: usize,
    /// True when `total` exceeds the returned rows.
    pub truncated: bool,
    /// True when this target has a font discovery mechanism.
    pub supported: bool,
}

impl FaceCatalog {
    fn empty(supported: bool) -> Self {
        Self {
            faces: Vec::new(),
            total: 0,
            truncated: false,
            supported,
        }
    }
}

/// Whether this build target has a system font discovery mechanism.
#[must_use]
pub const fn font_discovery_supported() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
    ))
}

/// Enumerates system faces, sorted and capped at `max_faces`.
///
/// `max_faces` is clamped to [`MAX_CATALOG_FACES`]; zero still reports the
/// unique `total` so callers can render `truncated` honestly. `user_font_dirs`
/// classifies a face as `user` when its file lives under one of the roots; no
/// root is required for discovery (the platform scan owns that).
#[must_use]
pub fn system_face_catalog(max_faces: usize, user_font_dirs: &[PathBuf]) -> FaceCatalog {
    if !font_discovery_supported() {
        return FaceCatalog::empty(false);
    }
    let cap = max_faces.min(MAX_CATALOG_FACES);
    let mut unique: BTreeSet<CatalogFace> = BTreeSet::new();
    let db = crate::shaped::system_db();
    for face in db.faces() {
        let Some(family) = face.families.first().map(|(name, _)| name.as_str()) else {
            continue;
        };
        let family = sanitize_label(family);
        if family.is_empty() {
            continue;
        }
        unique.insert(CatalogFace {
            family,
            style: style_label(face.weight, face.style),
            source: source_label(&face.source, user_font_dirs).to_string(),
        });
    }
    let total = unique.len();
    let faces: Vec<CatalogFace> = unique.into_iter().take(cap).collect();
    FaceCatalog {
        truncated: total > faces.len(),
        total,
        faces,
        supported: true,
    }
}

/// Maps a `fontdb` weight + slant onto the owned style vocabulary.
fn style_label(weight: Weight, style: DbStyle) -> String {
    let weight = weight_name(weight.0);
    match style {
        DbStyle::Normal => weight,
        DbStyle::Italic => format!("{weight} Italic"),
        DbStyle::Oblique => format!("{weight} Oblique"),
    }
}

/// Maps a numeric weight onto a stable label.
fn weight_name(weight: u16) -> String {
    let name = match weight {
        100 => "Thin",
        200 => "ExtraLight",
        300 => "Light",
        400 => "Regular",
        500 => "Medium",
        600 => "SemiBold",
        700 => "Bold",
        800 => "ExtraBold",
        900 => "Black",
        _ => return weight.to_string(),
    };
    name.to_string()
}

/// Classifies a face source without leaking the absolute path.
fn source_label(source: &Source, user_font_dirs: &[PathBuf]) -> &'static str {
    match source {
        Source::File(path) => {
            if user_font_dirs.iter().any(|root| path_is_under(path, root)) {
                "user"
            } else {
                "system"
            }
        }
        Source::Binary(_) => "embedded",
    }
}

/// `path.starts_with(root)` without touching the filesystem.
fn path_is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Scrubs control characters and caps a label on a character boundary.
fn sanitize_label(raw: &str) -> String {
    let trimmed = raw.trim();
    let mut out = String::with_capacity(trimmed.len().min(MAX_CATALOG_TEXT_BYTES));
    for ch in trimmed.chars() {
        if ch.is_control() {
            continue;
        }
        if out.len() + ch.len_utf8() > MAX_CATALOG_TEXT_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_labels_are_stable() {
        assert_eq!(style_label(Weight::NORMAL, DbStyle::Normal), "Regular");
        assert_eq!(style_label(Weight::BOLD, DbStyle::Italic), "Bold Italic");
        assert_eq!(style_label(Weight(350), DbStyle::Oblique), "350 Oblique");
    }

    #[test]
    fn labels_drop_control_bytes_and_cap_on_boundaries() {
        assert_eq!(
            sanitize_label("  Jet\u{1b}Brains\u{0} Mono  "),
            "JetBrains Mono"
        );
        let long = "é".repeat(MAX_CATALOG_TEXT_BYTES);
        let capped = sanitize_label(&long);
        assert!(capped.len() <= MAX_CATALOG_TEXT_BYTES);
        assert!(capped.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn unsupported_cap_still_reports_supported_target() {
        // The host running the test suite is supported; the catalog contract
        // (sorted, unique, capped) is asserted only when rows exist so the
        // test stays hermetic on a fontless CI image.
        let catalog = system_face_catalog(MAX_CATALOG_FACES, &[]);
        assert_eq!(catalog.supported, font_discovery_supported());
        assert!(catalog.faces.len() <= catalog.total);
        assert_eq!(catalog.truncated, catalog.total > catalog.faces.len());
        let mut sorted = catalog.faces.clone();
        sorted.sort();
        assert_eq!(sorted, catalog.faces);
        assert!(catalog.faces.iter().all(|face| {
            !face.family.is_empty()
                && face.family.len() <= MAX_CATALOG_TEXT_BYTES
                && face.style.len() <= MAX_CATALOG_TEXT_BYTES
                && !face.family.chars().any(char::is_control)
        }));
    }

    #[test]
    fn zero_cap_returns_no_rows_but_keeps_total() {
        let catalog = system_face_catalog(0, &[]);
        assert!(catalog.faces.is_empty());
        assert_eq!(catalog.truncated, catalog.total > 0);
    }
}
