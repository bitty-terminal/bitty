//! Embedded symbols-only Nerd face (issue #2931).
//!
//! `Source::Binary` support existed in [`font_catalog`](crate::font_catalog)
//! but nothing embedded a face, so the Nerd Font private-use icon range
//! (`U+E000-U+F8FF`) had no fallback on a stock Windows/macOS install: the
//! statusline, bar, and devtools icons painted tofu. Ghostty embeds a
//! symbols-only Nerd font and kitty ships a builtin one; this module does the
//! same for Bitty, without a runtime dependency on a host-installed Nerd font.
//!
//! # Asset
//!
//! `assets/bitty-symbols-nerd.ttf` is a subset of Nerd Fonts v3.5.1
//! `JetBrainsMonoNerdFont-Regular.ttf` renamed to [`EMBEDDED_SYMBOLS_FAMILY`]
//! (OFL section 3). Kept ranges: `U+E000-U+F8FF` (PUA icons),
//! `U+2500-U+257F` (box drawing), `U+2580-U+259F` (block elements), and
//! `U+2800-U+28FF` (braille patterns). Regeneration — source URL, pinned
//! SHA-256, tool, and ranges — lives in `assets/generate-symbols-font.sh`;
//! licensing lives in `assets/LICENSES.md`.
//!
//! # Chain position
//!
//! The face is registered in the process-wide font database and appended to
//! every platform chain *last* (see `bitty_config::types::FONT_FALLBACK_CHAIN`):
//! additive, so it never shadows a host face, and emoji-presentation scalars
//! still resolve through the platform emoji face first. Loading is lazy like
//! every other tail (the fallback walk only maps it on a miss), so an
//! ASCII-only session never pays for it.
//!
//! # Bounds and safety
//!
//! The bytes are compiled in (no filesystem or network read at runtime), the
//! face is parsed by the same bounded `fontdb`/`swash` path as every host
//! face, and coverage probes parse the in-memory bytes directly. No `unsafe`.

#![forbid(unsafe_code)]

use fontdb::Database;

/// Family name the embedded face is registered under.
///
/// Declared in `bitty-config` so every platform chain can name it without a
/// renderer dependency; re-exported here so render-side callers do not reach
/// across the crate boundary.
pub use bitty_config::types::EMBEDDED_SYMBOLS_FAMILY;

/// Raw face bytes compiled into the binary.
///
/// Kept as a `&'static [u8]` slice: `fontdb` clones it once into the shared
/// database as a `Source::Binary` face, and coverage probes borrow it.
pub const EMBEDDED_SYMBOLS_BYTES: &[u8] = include_bytes!("../assets/bitty-symbols-nerd.ttf");

/// First private-use scalar covered by the embedded face.
pub const PUA_FIRST: char = '\u{E000}';
/// Last private-use scalar in the Nerd Font icon range.
///
/// Not every scalar in the span is mapped (Nerd Fonts 3.5.1 has no glyph for
/// `U+F8FF`, the legacy Apple logo, for licensing reasons), but the range
/// bound is what callers reason about.
pub const PUA_LAST: char = '\u{F8FF}';

/// Registers the embedded face in `db`.
///
/// `fontdb` records it as a `Source::Binary` face, so
/// [`font_catalog`](crate::font_catalog) reports it as `embedded` and
/// [`shaped::SwashSingle`](crate::shaped::SwashSingle) loads it from memory
/// without a path. Idempotent for a given database: a second call appends a
/// duplicate face, so callers register exactly once at database construction.
pub(crate) fn register_face(db: &mut Database) {
    db.load_font_data(EMBEDDED_SYMBOLS_BYTES.to_vec());
}

/// True when the embedded face maps `c` to a glyph.
///
/// Parses the in-memory bytes with the same parser the rasterizer uses
/// (`swash`/`read-fonts`); a corrupt asset reports `false` instead of
/// panicking. Doctor and the coverage tests use this to state whether the
/// private-use icon range is covered.
#[must_use]
pub fn embedded_face_covers(c: char) -> bool {
    let Some(font) = swash::FontRef::from_index(EMBEDDED_SYMBOLS_BYTES, 0) else {
        return false;
    };
    swash::Charmap::from_font(&font).map(c) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_face_covers_the_icon_and_graph_ranges() {
        // Issue #2931 acceptance: the PUA range endpoints, a box-drawing
        // scalar, a block element, and a braille scalar all resolve in the
        // embedded face, so a stock Windows/macOS host paints icons instead
        // of tofu.
        assert!(embedded_face_covers(PUA_FIRST), "U+E000 must be covered");
        assert!(
            embedded_face_covers('\u{F017}'),
            "U+F017 (fa-clock) must be covered"
        );
        assert!(
            embedded_face_covers('\u{E0B0}'),
            "U+E0B0 (powerline separator) must be covered"
        );
        assert!(embedded_face_covers('\u{2500}'), "box drawing U+2500");
        assert!(embedded_face_covers('\u{2588}'), "block U+2588");
        assert!(embedded_face_covers('\u{28FF}'), "braille U+28FF");
        // The range bound is documented as a span, not a guarantee: Nerd
        // Fonts 3.5.1 carries no U+F8FF glyph.
        assert!(
            !embedded_face_covers(PUA_LAST),
            "U+F8FF is not in Nerd Fonts 3.5.1"
        );
        // A scalar outside every kept range stays uncovered.
        assert!(!embedded_face_covers('A'));
    }

    #[test]
    fn embedded_face_rasterizes_a_pua_icon_from_memory() {
        // End-to-end through the real rasterizer: an empty host database
        // plus the embedded face must load it as a `Source::Binary` primary
        // and rasterize a PUA icon. Host-independent (no system fonts).
        use crate::glyph::{FontQuery, FontStyle, GlyphRasterizer, RasterKey};
        let mut db = Database::new();
        register_face(&mut db);
        let mut shaped = crate::shaped::SwashSingle::with_database(std::sync::Arc::new(db));
        let query = FontQuery {
            family: EMBEDDED_SYMBOLS_FAMILY.to_string(),
            style: FontStyle::Normal,
            point_size: 12.0,
        };
        let font = shaped.load_font(&query).expect("embedded face must load");
        assert_eq!(
            shaped.loaded_face_family(font).as_deref(),
            Some(EMBEDDED_SYMBOLS_FAMILY)
        );
        for c in [PUA_FIRST, '\u{E0B0}', '\u{2500}', '\u{28FF}'] {
            let key = RasterKey::new(c, font, 12.0).expect("valid raster key");
            let bitmap = shaped.rasterize(key).expect("rasterize");
            assert!(bitmap.is_some(), "U+{:04X} must rasterize", u32::from(c));
        }
    }

    #[test]
    fn embedded_face_is_registered_as_a_binary_source() {
        let mut db = Database::new();
        register_face(&mut db);
        let families = [fontdb::Family::Name(EMBEDDED_SYMBOLS_FAMILY)];
        let query = fontdb::Query {
            families: &families,
            weight: fontdb::Weight::NORMAL,
            style: fontdb::Style::Normal,
            stretch: fontdb::Stretch::Normal,
        };
        let id = db.query(&query).expect("embedded family must resolve");
        let info = db.face(id).expect("embedded face info");
        assert!(
            info.families
                .iter()
                .any(|(name, _)| name == EMBEDDED_SYMBOLS_FAMILY),
            "registered family name must match the chain constant"
        );
        assert!(
            matches!(db.face_source(id), Some((fontdb::Source::Binary(_), _))),
            "embedded face must be a binary source"
        );
    }
}
