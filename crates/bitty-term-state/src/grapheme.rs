//! UAX #29 grapheme-cluster segmentation over terminal text (M1, #1146).
//!
//! Candidate implementation of the cluster-segmentation half of the text
//! domain: where [`crate::char_cell_width`] answers "how many cells does
//! this scalar occupy", this module answers "which scalars travel together".
//! The segmenter is dependency-free, deterministic, and total over any
//! `&str`: no I/O, no allocation beyond the returned slices, no platform
//! variance. The East_Asian_Width and Extended_Pictographic lookup tables are
//! generated from pinned UCD 18.0.0 snapshots (`crate::width_tables`); the
//! remaining segmentation property subsets are curated and documented below,
//! and the full official `GraphemeBreakTest.txt` passes. Unlisted scalars
//! resolve to the fail-open `GB999 Any ÷ Any` break, never to a panic or an
//! over-join.
//!
//! Rule coverage (UAX #29, grapheme-cluster boundary rules):
//!
//! - `GB3` (`CR × LF`), `GB4`/`GB5` (break around controls): full.
//! - `GB6`–`GB8` (Hangul syllables): Jamo `1100..11FF`, extended Jamo
//!   `A960..A97C`/`D7B0..D7FB`, and precomposed syllables `AC00..D7A3`.
//! - `GB9` (`× Extend | ZWJ`): [`is_grapheme_extend`] over the crate
//!   zero-width tables plus emoji modifiers `1F3FB..1F3FF`; ZWNJ `200C`
//!   joins as Extend. Other format controls (bidi marks, Arabic format
//!   controls, `FEFF`, tags `E0000..E0001`) break, matching their `Control`
//!   grapheme property.
//! - `GB9a` (`× SpacingMark`): curated Indic, Thai/Lao, Myanmar, and Khmer
//!   spacing-mark ranges. Unlisted spacing marks break (table gap).
//! - `GB9b` (`Prepend × Any`): the Unicode 18.0.0 `Prepend` class.
//! - `GB11` (extended-pictographic ZWJ sequences): the authoritative UTS #51
//!   `Extended_Pictographic` set generated from the pinned `emoji-data.txt`.
//! - `GB12`/`GB13` (regional-indicator pairing): full, with run parity.
//! - Indic conjuncts (`GB9c`, virama linkers): linker state tracked across
//!   `InCB=Extend` scalars, closing on a following Indic/Tibetan/Myanmar/
//!   Khmer/Balinese/Vedic letter. The `InCB` consonant requirement before
//!   the linker is not tracked (documented M1-lite approximation).
//!
//! Tibetan subjoining beyond the combining vowel-sign range `0F72..0F84`
//! segments conservatively (breaks where a full `InCB` table would join);
//! that is a table gap, stated here so no reviewer mistakes it for shaping.
//!
//! [`Graphemes`] yields the clusters of a string in order; [`split_clusters`]
//! collects them. [`cluster_cell_width`] measures one cluster for grid work:
//! Wide/Fullwidth scalars and emoji presentation make the cluster occupy two
//! cells, regional-indicator pairs occupy two, and every other cluster one
//! (`0` for a zero-width-only cluster). The state print path stores a
//! cluster on one leading cell plus, for a two-cell cluster, one wide
//! spacer, and [`cluster_continues`] decides whether the next scalar joins
//! that cell (CTX-1609).

use super::cell::{AmbiguousWidth, char_cell_width_in, is_zero_width};
use super::width_tables::is_extended_pictographic_scalar;

/// Zero-width joiner: joins backward (`GB9`), and forward only across an
/// extended-pictographic run (`GB11`).
const ZWJ: u32 = 0x200D;

/// Zero-width non-joiner: joins as `Extend` backward (`GB9`) but suppresses
/// the `GB9c` conjunct continuation.
const ZWNJ: u32 = 0x200C;

/// Variation selector-16: requests emoji presentation for the preceding
/// base, widening a text-default pictograph or keycap cluster to two cells.
const VARIATION_SELECTOR_16: char = '\u{FE0F}';

/// Combining enclosing keycap: the third scalar of a keycap sequence
/// (`#` + VS16 + U+20E3), which renders as one two-cell emoji.
const COMBINING_KEYCAP: char = '\u{20E3}';

/// Carriage return / line feed (`GB3`).
const CR: u32 = 0x000D;
const LF: u32 = 0x000A;

/// Whether `cp` breaks grapheme runs on both sides (`GB4`/`GB5`).
///
/// C0/C1 controls, `ZL`/`Zp` line separators, tag prefix scalars, and the
/// format controls UAX classes as `Control` (bidi marks, Arabic format
/// controls, `FEFF`). ZWJ is excluded: it has its own join rule.
fn is_break_control(cp: u32) -> bool {
    matches!(cp,
        0x0000..=0x001F
        | 0x007F..=0x009F
        | 0x061C
        | 0x200B
        | 0x200E..=0x200F
        | 0x2028..=0x202E
        | 0x2060..=0x2064
        | 0xFEFF
        | 0xE0000..=0xE0001
    )
}

/// Whether `cp` is `Prepend` in the UAX #29 table (`GB9b`).
///
/// Prepend format controls join forward to the following scalar. The set is
/// the Unicode 18.0.0 `GraphemeBreakProperty.txt` `Prepend` class; these
/// scalars are zero-width in the cell tables, so the grid drops a leading
/// one but never breaks a following cluster.
fn is_prepend(cp: u32) -> bool {
    matches!(cp,
        0x0600..=0x0605
        | 0x06DD
        | 0x070F
        | 0x0890..=0x0891
        | 0x08E2
        | 0x0D4E
        | 0x110BD
        | 0x110CD
        | 0x111C2..=0x111C3
        | 0x113D1
        | 0x1193F
        | 0x11941
        | 0x11A84..=0x11A89
        | 0x11D46
        | 0x11F02
    )
}

/// Whether `cp` is `Grapheme_Extend` in the crate table (`GB9`).
///
/// The crate zero-width tables minus the format controls (which break, see
/// `is_break_control`), minus `Prepend` scalars (own forward rule, `GB9b`),
/// and minus ZWJ (own class), plus emoji modifiers `1F3FB..1F3FF` (wide
/// scalars that still join forward). ZWNJ `U+200C` joins as `Extend`, per
/// UAX #29.
#[must_use]
pub fn is_grapheme_extend(cp: u32) -> bool {
    if cp == ZWJ || is_break_control(cp) || is_prepend(cp) {
        return false;
    }
    if matches!(cp, 0x1F3FB..=0x1F3FF) {
        return true;
    }
    is_zero_width(cp)
}

/// Whether `cp` is a spacing mark that joins backward (`GB9a`).
///
/// Curated `SpacingMark` subset: Devanagari, Bengali, Gurmukhi, Gujarati,
/// Oriya, Tamil, Telugu, Kannada, Malayalam, Sinhala vowel signs, Thai
/// `0E33`, Lao `0EB3`/`0EBD`, Myanmar `1031`/`103B..103C`/`1056..1057`/
/// `1084`, Balinese `1B04`, Khmer `17B6`/`17BE..17C5`/`17C7..17C8`. Unlisted
/// spacing marks break; the official UCD fixture does not exercise the
/// remaining gap.
fn is_spacing_mark(cp: u32) -> bool {
    matches!(cp,
        0x0903
        | 0x093B
        | 0x093E..=0x0940
        | 0x0949..=0x094C
        | 0x094E..=0x094F
        | 0x09BE..=0x09C4
        | 0x09C7..=0x09C8
        | 0x09CB..=0x09CC
        | 0x0A03
        | 0x0A3E..=0x0A42
        | 0x0A83
        | 0x0ABE..=0x0AC5
        | 0x0AC7..=0x0AC9
        | 0x0ACB..=0x0ACC
        | 0x0B02..=0x0B03
        | 0x0B3E..=0x0B44
        | 0x0B47..=0x0B48
        | 0x0B4B..=0x0B4C
        | 0x0BBE..=0x0BC2
        | 0x0BC6..=0x0BC8
        | 0x0BCA..=0x0BCC
        | 0x0C01..=0x0C03
        | 0x0C3E..=0x0C44
        | 0x0C46..=0x0C48
        | 0x0C4A..=0x0C4C
        | 0x0C82..=0x0C83
        | 0x0CBE..=0x0CC4
        | 0x0CC6..=0x0CC8
        | 0x0CCA..=0x0CCC
        | 0x0D02..=0x0D03
        | 0x0D3E..=0x0D44
        | 0x0D46..=0x0D48
        | 0x0D4A..=0x0D4C
        | 0x0DCF..=0x0DD4
        | 0x0DD6
        | 0x0DD8..=0x0DDF
        | 0x0E33
        | 0x0EB3
        | 0x0EBD
        | 0x1031
        | 0x103B..=0x103C
        | 0x1056..=0x1057
        | 0x1084
        | 0x17B6
        | 0x17BE..=0x17C5
        | 0x17C7..=0x17C8
        | 0x1B04
    )
}

/// Whether `cp` may form an emoji ZWJ sequence (`GB11`).
///
/// The authoritative UTS #51 `Extended_Pictographic` set from the pinned
/// `emoji-data.txt` snapshot (UCD 18.0.0, generated into
/// [`crate::width_tables`]). The earlier curated subset omitted text-default
/// pictographs such as U+2764, so sequences like heart-on-fire split.
#[must_use]
pub fn is_extended_pictographic(cp: u32) -> bool {
    is_extended_pictographic_scalar(cp)
}

/// Whether `cp` is a regional indicator (`GB12`/`GB13`).
#[must_use]
pub fn is_regional_indicator(cp: u32) -> bool {
    matches!(cp, 0x1F1E6..=0x1F1FF)
}

/// `InCB=Linker` scalars for the conjunct rule (`GB9c`).
///
/// The Unicode 18.0.0 `DerivedCoreProperties.txt` `InCB; Linker` set:
/// Devanagari, Bengali, Gujarati, Oriya, Telugu, Malayalam, Myanmar, Khmer,
/// Tai Tham, Balinese, Zanabazar Square, Vedic, Javanese, Cham, Tai Viet,
/// and the Kaithi/Sharada/Khojki/Tirhuta/Modi/Soyombo/Bhaiksuki linkers.
/// A linker opens a conjunct run that a following consonant closes, with
/// `InCB=Extend` scalars allowed in between.
fn is_linker(cp: u32) -> bool {
    matches!(
        cp,
        0x094D
            | 0x09CD
            | 0x0ACD
            | 0x0B4D
            | 0x0C4D
            | 0x0D4D
            | 0x1039
            | 0x17D2
            | 0x1A60
            | 0x1B44
            | 0x1BAB
            | 0x1CF5
            ..=0x1CF6
                | 0xA9C0
                | 0xAAF6
                | 0x10A3F
                | 0x11133
                | 0x113D0
                | 0x1193E
                | 0x11A3A
                | 0x11A47
                | 0x11A99
                | 0x11F42
    )
}

/// Letters the linker rule may join onto (Indic blocks, Tibetan, Myanmar,
/// Khmer, Tai Tham, Balinese, Vedic, and the Brahmic supplement blocks):
/// any scalar in these ranges that is not itself a joiner, control, or
/// indicator.
fn is_linker_target(cp: u32) -> bool {
    if is_grapheme_extend(cp)
        || is_spacing_mark(cp)
        || is_break_control(cp)
        || is_regional_indicator(cp)
        || is_linker(cp)
        || cp == ZWJ
    {
        return false;
    }
    matches!(
        cp,
        0x0900..=0x0DFF
            | 0x0F00..=0x0FFF
            | 0x1000..=0x109F
            | 0x1700..=0x1CFF
            | 0x11000..=0x11FFF
    )
}

/// Hangul syllable class for `GB6`–`GB8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hangul {
    Leading,
    Vowel,
    Trailing,
    LvSyllable,
    LvtSyllable,
    Other,
}

fn hangul_class(cp: u32) -> Hangul {
    match cp {
        0x1100..=0x115F | 0xA960..=0xA97C => Hangul::Leading,
        0x1160..=0x11A7 | 0xD7B0..=0xD7C6 => Hangul::Vowel,
        0x11A8..=0x11FF | 0xD7CB..=0xD7FB => Hangul::Trailing,
        0xAC00..=0xD7A3 if (cp - 0xAC00) % 28 == 0 => Hangul::LvSyllable,
        0xAC00..=0xD7A3 => Hangul::LvtSyllable,
        _ => Hangul::Other,
    }
}

/// Unbroken-run state carried between scalars of one cluster.
#[derive(Debug, Clone, Copy)]
struct RunState {
    /// Previous scalar.
    prev: u32,
    /// Previous Hangul class (`GB6`–`GB8`).
    prev_hangul: Hangul,
    /// Whether an extended-pictographic run is open for `GB11`: an
    /// `Extended_Pictographic` scalar was seen with only `Extend` or
    /// spacing-mark scalars since (a break resets it).
    pictographic_open: bool,
    /// Whether the previous scalar is ZWJ (second half of `GB11`).
    prev_is_zwj: bool,
    /// Whether an `InCB=Linker` was seen in this run with only
    /// `InCB=Extend` scalars since (`GB9c`).
    linker_open: bool,
    /// Regional indicators already in this run (`GB12`/`GB13` parity).
    ri_run: u32,
}

impl RunState {
    fn start(prev: u32) -> Self {
        Self {
            prev,
            prev_hangul: hangul_class(prev),
            pictographic_open: is_extended_pictographic(prev),
            prev_is_zwj: prev == ZWJ,
            linker_open: is_linker(prev),
            ri_run: u32::from(is_regional_indicator(prev)),
        }
    }

    /// Advances the run state over one scalar that does not break.
    fn advance(self, cp: u32) -> Self {
        // ZWJ keeps a pictographic run open (`GB11` spans ExtPict
        // (Extend | spacing-mark)* ZWJ); anything else closes it unless it
        // is itself pictographic or a joiner.
        let extend_like = cp == ZWJ || is_grapheme_extend(cp) || is_spacing_mark(cp);
        Self {
            prev: cp,
            prev_hangul: hangul_class(cp),
            pictographic_open: if is_extended_pictographic(cp) {
                true
            } else {
                self.pictographic_open && extend_like
            },
            prev_is_zwj: cp == ZWJ,
            linker_open: if cp == ZWNJ {
                // ZWNJ is InCB=Extend but explicitly suppresses a
                // conjunct, so it closes the linker run without breaking
                // the grapheme itself.
                false
            } else if is_linker(cp) {
                true
            } else if extend_like {
                self.linker_open
            } else {
                false
            },
            ri_run: if is_regional_indicator(cp) {
                self.ri_run + 1
            } else {
                0
            },
        }
    }
}

/// Whether a break stands between the run ending in `state` and `current`
/// (UAX #29 rules in spec order).
fn breaks_before(state: &RunState, current: u32) -> bool {
    let prev = state.prev;
    // GB3: CR × LF.
    if prev == CR && current == LF {
        return false;
    }
    // GB4 / GB5: break around controls.
    if is_break_control(prev) || is_break_control(current) {
        return true;
    }
    // GB6: L × (L | V | LV | LVT).
    if state.prev_hangul == Hangul::Leading {
        let next = hangul_class(current);
        if matches!(
            next,
            Hangul::Leading | Hangul::Vowel | Hangul::LvSyllable | Hangul::LvtSyllable
        ) {
            return false;
        }
    }
    // GB7: (LV | V) × (V | T).
    if matches!(state.prev_hangul, Hangul::LvSyllable | Hangul::Vowel)
        && matches!(hangul_class(current), Hangul::Vowel | Hangul::Trailing)
    {
        return false;
    }
    // GB8: (LVT | T) × T.
    if matches!(state.prev_hangul, Hangul::LvtSyllable | Hangul::Trailing)
        && hangul_class(current) == Hangul::Trailing
    {
        return false;
    }
    // GB9: × (Extend | ZWJ).
    if is_grapheme_extend(current) || current == ZWJ {
        return false;
    }
    // GB9a: × SpacingMark.
    if is_spacing_mark(current) {
        return false;
    }
    // GB9b: Prepend × Any.
    if is_prepend(prev) {
        return false;
    }
    // GB11: ExtPict Extend* ZWJ × ExtPict.
    if state.prev_is_zwj && state.pictographic_open && is_extended_pictographic(current) {
        return false;
    }
    // GB12 / GB13: keep regional indicators paired.
    if is_regional_indicator(prev) && is_regional_indicator(current) {
        return state.ri_run % 2 == 0;
    }
    // GB9c (M1-lite): a linker run closes on an Indic-block letter.
    if state.linker_open && is_linker_target(current) {
        return false;
    }
    // GB999: Any ÷ Any.
    true
}

/// Grapheme clusters of `text` in order (UAX #29 M1 subset).
///
/// The iterator is total: it yields every byte of `text` exactly once, so
/// concatenating the clusters reproduces the input. Empty input yields no
/// clusters.
#[derive(Debug, Clone)]
pub struct Graphemes<'a> {
    text: &'a str,
    byte: usize,
}

impl<'a> Graphemes<'a> {
    /// Clusters of `text` in order.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self { text, byte: 0 }
    }
}

impl<'a> Iterator for Graphemes<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.text.get(self.byte..)?;
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        let mut end = self.byte + first.len_utf8();
        let mut state = RunState::start(first as u32);
        for (offset, current) in chars {
            let cp = current as u32;
            if breaks_before(&state, cp) {
                break;
            }
            end = self.byte + offset + current.len_utf8();
            state = state.advance(cp);
        }
        let cluster = self.text.get(self.byte..end)?;
        self.byte = end;
        Some(cluster)
    }
}

/// Collects the grapheme clusters of `text` in order.
#[must_use]
pub fn split_clusters(text: &str) -> Vec<&str> {
    Graphemes::new(text).collect()
}

/// Cell width of one grapheme cluster under `ambiguous`.
///
/// A cluster occupies two cells when it contains a Wide/Fullwidth scalar
/// (which includes emoji-presentation scalars), when it is a
/// regional-indicator pair, or when a variation selector-16 turns an
/// extended-pictographic base or keycap sequence into emoji presentation;
/// every other non-empty cluster occupies one cell. An all-zero-width
/// cluster (a lone combining mark or lone ZWJ) occupies none.
#[must_use]
pub fn cluster_cell_width(cluster: &str, ambiguous: AmbiguousWidth) -> u8 {
    cluster_width(cluster.chars(), ambiguous)
}

/// [`cluster_cell_width`] over a leading glyph plus its stored tail.
///
/// The state print path keeps the base scalar in `Cell::glyph` and the
/// remaining cluster scalars in the bounded continuation buffer; this
/// measures that pair without rebuilding a `String`.
#[must_use]
pub(crate) fn cluster_cell_width_parts(
    glyph: char,
    tail: &[char],
    ambiguous: AmbiguousWidth,
) -> u8 {
    cluster_width(
        std::iter::once(glyph).chain(tail.iter().copied()),
        ambiguous,
    )
}

fn cluster_width(chars: impl Iterator<Item = char>, ambiguous: AmbiguousWidth) -> u8 {
    let mut max_width = 0u8;
    let mut regional = 0u8;
    let mut emoji_base = false;
    let mut variation_16 = false;
    let mut keycap = false;
    for scalar in chars {
        max_width = max_width.max(char_cell_width_in(scalar, ambiguous));
        if is_regional_indicator(scalar as u32) {
            regional = regional.saturating_add(1);
        }
        if scalar == VARIATION_SELECTOR_16 {
            variation_16 = true;
        }
        if scalar == COMBINING_KEYCAP {
            keycap = true;
        }
        if is_extended_pictographic(scalar as u32) {
            emoji_base = true;
        }
    }
    if max_width == 0 {
        return 0;
    }
    if max_width == 2 || regional >= 2 || (variation_16 && (emoji_base || keycap)) {
        return 2;
    }
    1
}

/// Whether `next` extends the stored cluster (`glyph` plus `tail`) instead
/// of starting a new one under the UAX #29 rules this module implements.
///
/// Used by the state print path: a scalar that does not break joins the
/// preceding cell's cluster and therefore does not consume a new layout
/// unit. The tail is one cluster by construction; a malformed buffer that
/// already contains a break refuses the join (fail-open to per-scalar
/// layout, never an over-join).
#[must_use]
pub(crate) fn cluster_continues(glyph: char, tail: &[char], next: char) -> bool {
    let mut state = RunState::start(glyph as u32);
    for &scalar in tail {
        if breaks_before(&state, scalar as u32) {
            return false;
        }
        state = state.advance(scalar as u32);
    }
    !breaks_before(&state, next as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clusters(text: &str) -> Vec<&str> {
        split_clusters(text)
    }

    #[test]
    fn ascii_splits_per_scalar_and_rejoins() {
        assert_eq!(clusters("hello"), vec!["h", "e", "l", "l", "o"]);
        assert!(clusters("").is_empty());
    }

    #[test]
    fn combining_marks_join_backward() {
        assert_eq!(clusters("e\u{0301}"), vec!["e\u{0301}"]);
        assert_eq!(
            clusters("a\u{0300}\u{0301}b"),
            vec!["a\u{0300}\u{0301}", "b"]
        );
    }

    #[test]
    fn crlf_stays_together_controls_break() {
        assert_eq!(clusters("\r\n"), vec!["\r\n"]);
        assert_eq!(clusters("\r\r"), vec!["\r", "\r"]);
        assert_eq!(clusters("a\nb"), vec!["a", "\n", "b"]);
        assert_eq!(clusters("a\u{200B}b"), vec!["a", "\u{200B}", "b"]);
    }

    #[test]
    fn hangul_syllables_hold_together() {
        // L × V, LV alone, LV × T, LVT × T.
        assert_eq!(clusters("가"), vec!["가"]);
        assert_eq!(clusters("가"), vec!["가"]);
        assert_eq!(clusters("각"), vec!["각"]);
        assert_eq!(clusters("각ᆨ"), vec!["각ᆨ"]);
        assert_eq!(clusters("가나"), vec!["가", "나"]);
    }

    #[test]
    fn zwj_emoji_sequences_are_one_cluster() {
        let family = "👩\u{200D}👩\u{200D}👧";
        assert_eq!(clusters(family), vec![family]);
        let thumbs = "👍\u{1F3FD}";
        assert_eq!(clusters(thumbs), vec![thumbs]);
        // ZWJ joins backward but breaks forward outside pictographs.
        assert_eq!(clusters("a\u{200D}b"), vec!["a\u{200D}", "b"]);
    }

    #[test]
    fn regional_indicators_pair_up() {
        let flag = "🇫🇷";
        assert_eq!(clusters(flag), vec![flag]);
        let three = "🇫🇷🇫";
        assert_eq!(clusters(three).len(), 2);
        assert_eq!(clusters("a🇫b"), vec!["a", "🇫", "b"]);
    }

    #[test]
    fn indic_conjuncts_hold_together() {
        // Spacing mark joins; virama joins both sides (M1-lite GB9c).
        assert_eq!(clusters("क\u{093F}"), vec!["क\u{093F}"]);
        assert_eq!(clusters("क\u{094D}ष"), vec!["क\u{094D}ष"]);
    }

    #[test]
    fn keycap_sequence_is_one_cluster() {
        let keycap = "#\u{FE0F}\u{20E3}";
        assert_eq!(clusters(keycap), vec![keycap]);
    }

    #[test]
    fn cluster_width_follows_cluster_presentation() {
        assert_eq!(cluster_cell_width("", AmbiguousWidth::Narrow), 0);
        assert_eq!(cluster_cell_width("a", AmbiguousWidth::Narrow), 1);
        assert_eq!(cluster_cell_width("中", AmbiguousWidth::Narrow), 2);
        assert_eq!(cluster_cell_width("e\u{0301}", AmbiguousWidth::Narrow), 1);
        // A lone zero-width cluster owns no cell.
        assert_eq!(cluster_cell_width("\u{0301}", AmbiguousWidth::Narrow), 0);
        assert_eq!(cluster_cell_width("\u{200D}", AmbiguousWidth::Narrow), 0);
        let family = "👩\u{200D}👩\u{200D}👧";
        assert_eq!(cluster_cell_width(family, AmbiguousWidth::Narrow), 2);
        // Regional-indicator pair, skin tone, VS16 heart, keycap.
        assert_eq!(
            cluster_cell_width("\u{1F1EB}\u{1F1F7}", AmbiguousWidth::Narrow),
            2
        );
        assert_eq!(
            cluster_cell_width("\u{1F44D}\u{1F3FD}", AmbiguousWidth::Narrow),
            2
        );
        assert_eq!(
            cluster_cell_width("\u{2764}\u{FE0F}", AmbiguousWidth::Narrow),
            2
        );
        assert_eq!(
            cluster_cell_width("#\u{FE0F}\u{20E3}", AmbiguousWidth::Narrow),
            2
        );
        // Text presentation stays narrow.
        assert_eq!(
            cluster_cell_width("\u{2764}\u{FE0E}", AmbiguousWidth::Narrow),
            1
        );
        // Ambiguous policy affects the cluster like its scalars.
        assert_eq!(cluster_cell_width("α", AmbiguousWidth::Narrow), 1);
        assert_eq!(cluster_cell_width("α", AmbiguousWidth::Wide), 2);
    }

    #[test]
    fn extend_predicate_splits_joiners_from_breakers() {
        assert!(is_grapheme_extend(0x0301));
        assert!(is_grapheme_extend(0xFE0F));
        assert!(is_grapheme_extend(0x1F3FD));
        assert!(!is_grapheme_extend(ZWJ));
        assert!(!is_grapheme_extend(0x200B));
        assert!(!is_grapheme_extend(0x000A));
        assert!(is_extended_pictographic(0x1F600));
        assert!(!is_extended_pictographic(0x0041));
        assert!(is_regional_indicator(0x1F1EB));
        assert!(!is_regional_indicator(0x1F600));
    }

    /// Parses one line of the UCD `GraphemeBreakTest.txt` fixture into the
    /// full input string and the expected cluster strings.
    fn parse_break_test(line: &str) -> Option<(String, Vec<String>)> {
        let data = line.split('#').next()?.trim();
        if data.is_empty() {
            return None;
        }
        let mut text = String::new();
        let mut expected = Vec::new();
        let mut current = String::new();
        for token in data.split_whitespace() {
            match token {
                "÷" => {
                    if !current.is_empty() {
                        expected.push(std::mem::take(&mut current));
                    }
                }
                "×" => {}
                hex => {
                    let cp = u32::from_str_radix(hex, 16).ok()?;
                    let scalar = char::from_u32(cp)?;
                    text.push(scalar);
                    current.push(scalar);
                }
            }
        }
        Some((text, expected))
    }

    /// UCD 18.0.0 `GraphemeBreakTest.txt` conformance.
    ///
    /// The full official file is checked in and every sample must match
    /// exactly. Rule coverage is the module's documented set: GB3-GB9c,
    /// GB11-GB13, GB999. The `InCB=Consonant` requirement before a linker is
    /// not tracked (the linker state is forward-only), but no official sample
    /// distinguishes that approximation; regenerating the fixture for a
    /// newer UCD requires re-running this test before accepting the bump.
    #[test]
    fn ucd_grapheme_break_test_conformance() {
        const FIXTURE: &str = include_str!("../tests/data/GraphemeBreakTest-18.0.0.txt");
        let mut failures = Vec::new();
        for (index, line) in FIXTURE.lines().enumerate() {
            let Some((text, expected)) = parse_break_test(line) else {
                continue;
            };
            let actual: Vec<String> = split_clusters(&text)
                .into_iter()
                .map(str::to_string)
                .collect();
            if actual != expected {
                failures.push((index as u32 + 1, text, expected, actual));
            }
        }
        if !failures.is_empty() {
            for (line, text, expected, actual) in failures.iter().take(20) {
                eprintln!("line {line}: {text:?}\n  expected {expected:?}\n  actual   {actual:?}");
            }
            panic!(
                "{} of the UCD samples diverge from the M1 segmenter",
                failures.len()
            );
        }
    }
}
