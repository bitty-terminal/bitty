//! UAX #29 grapheme-cluster segmentation over terminal text (M1, #1146).
//!
//! Candidate implementation of the cluster-segmentation half of the text
//! domain: where [`crate::char_cell_width`] answers "how many cells does
//! this scalar occupy", this module answers "which scalars travel together".
//! The segmenter is dependency-free, deterministic, and total over any
//! `&str`: no I/O, no allocation beyond the returned slices, no platform
//! variance. Every property this module reads is generated from pinned UCD
//! 18.0.0 snapshots (`crate::unicode_tables`): `East_Asian_Width`,
//! `Extended_Pictographic`, `Grapheme_Cluster_Break`, and
//! `Indic_Conjunct_Break`. Nothing here is a curated subset, and the full
//! official `GraphemeBreakTest.txt` passes. Unlisted scalars resolve to the
//! fail-open `GB999 Any ÷ Any` break, never to a panic or an over-join.
//!
//! Rule coverage (UAX #29, grapheme-cluster boundary rules):
//!
//! - `GB3` (`CR × LF`), `GB4`/`GB5` (break around controls): full.
//! - `GB6`–`GB8` (Hangul syllables): Jamo `1100..11FF`, extended Jamo
//!   `A960..A97C`/`D7B0..D7FB`, and precomposed syllables `AC00..D7A3`.
//! - `GB9` (`× Extend | ZWJ`): [`is_grapheme_extend`] is the full generated
//!   `Grapheme_Cluster_Break=Extend` class (2274 scalars in UCD 18.0.0:
//!   combining marks, `ZWNJ`, variation selectors, tags, emoji modifiers);
//!   ZWJ joins through its own class.
//! - `GB9a` (`× SpacingMark`): the full generated `SpacingMark` class (381
//!   scalars: Indic, Thai/Lao, Myanmar, Khmer, Limbu, Tai Tham, ...).
//! - `GB9b` (`Prepend × Any`): the full generated `Prepend` class (27
//!   scalars), which the cell tables also treat as zero-width.
//! - `GB11` (extended-pictographic ZWJ sequences): the authoritative UTS #51
//!   `Extended_Pictographic` set generated from the pinned `emoji-data.txt`.
//! - `GB12`/`GB13` (regional-indicator pairing): full, with run parity.
//! - `GB9c` (Indic conjuncts): the full generated `Indic_Conjunct_Break`
//!   table, following the Unicode 18.0.0 rule shape
//!   `InCB=Linker InCB=Extend* × InCB=Consonant`. Only the script's
//!   `InCB=Linker` (virama) opens the join, only `InCB=Extend` scalars carry
//!   it, and only an `InCB=Consonant` closes it, so `क्१` (virama then a
//!   digit) and a virama followed by `ZWNJ` do not over-join.
//!
//! Every rule above is table-exact; there is no documented approximation
//! left in the segmenter.
//!
//! [`Graphemes`] yields the clusters of a string in order; [`split_clusters`]
//! collects them. [`cluster_cell_width`] measures one cluster for grid work:
//! Wide/Fullwidth scalars and emoji presentation make the cluster occupy two
//! cells, regional-indicator pairs occupy two, and every other cluster one
//! (`0` for a zero-width-only cluster). The state print path stores a
//! cluster on one leading cell plus, for a two-cell cluster, one wide
//! spacer, and `cluster_continues` decides whether the next scalar joins
//! that cell (CTX-1609).

use super::cell::{AmbiguousWidth, char_cell_width_in};
use super::unicode_tables::is_extended_pictographic_scalar;

/// Zero-width joiner: joins backward (`GB9`), and forward only across an
/// extended-pictographic run (`GB11`).
const ZWJ: u32 = 0x200D;

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
/// the generated Unicode 18.0.0 `GraphemeBreakProperty.txt` `Prepend` class
/// (`crate::unicode_tables::prepend`, 27 scalars); `crate::cell::is_zero_width`
/// reads the same table, so a prepend never consumes a cell and never hides
/// the base it joins to.
fn is_prepend(cp: u32) -> bool {
    crate::unicode_tables::prepend(cp)
}

/// Whether `cp` is `Grapheme_Cluster_Break=Extend` in the UAX #29 table
/// (`GB9`).
///
/// The full generated class (2274 scalars in UCD 18.0.0): combining marks,
/// `ZWNJ` `U+200C`, variation selectors, tag scalars, and emoji modifiers.
/// ZWJ is its own class (`GB9` names it separately) and never reports as
/// `Extend` here; `Control` scalars break (`GB4`/`GB5`) and the classes are
/// disjoint by construction.
#[must_use]
pub fn is_grapheme_extend(cp: u32) -> bool {
    cp != ZWJ && crate::unicode_tables::grapheme_extend(cp)
}

/// Whether `cp` is a spacing mark that joins backward (`GB9a`).
///
/// The full generated `SpacingMark` class (381 scalars in UCD 18.0.0):
/// Indic, Thai/Lao, Myanmar, Khmer, Limbu, Tai Tham, Balinese, and every
/// other spacing mark the UCD lists. Unlisted scalars break (`GB999`).
fn is_spacing_mark(cp: u32) -> bool {
    crate::unicode_tables::spacing_mark(cp)
}

/// Whether `cp` may form an emoji ZWJ sequence (`GB11`).
///
/// The authoritative UTS #51 `Extended_Pictographic` set from the pinned
/// `emoji-data.txt` snapshot (UCD 18.0.0, generated into
/// `crate::unicode_tables`). The earlier curated subset omitted text-default
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
/// The generated Unicode 18.0.0 `DerivedCoreProperties.txt`
/// `InCB; Linker` set (22 scalars): the viramas of Devanagari, Bengali,
/// Gujarati, Oriya, Telugu, Malayalam, Myanmar, Khmer, Tai Tham, Balinese,
/// Zanabazar Square, Vedic, Javanese, Cham, Tai Viet, and the
/// Kaithi/Sharada/Khojki/Tirhuta/Modi/Soyombo/Bhaiksuki linkers. A linker
/// only joins inside a conjunct opened by `InCB=Consonant`.
fn is_incb_linker(cp: u32) -> bool {
    crate::unicode_tables::incb_linker(cp)
}

/// `InCB=Consonant` scalars for the conjunct rule (`GB9c`).
///
/// The generated `InCB; Consonant` set (913 scalars): the consonants of the
/// scripts that define conjunct clusters.
fn is_incb_consonant(cp: u32) -> bool {
    crate::unicode_tables::incb_consonant(cp)
}

/// `InCB=Extend` scalars for the conjunct rule (`GB9c`).
///
/// The generated `InCB; Extend` set (2254 scalars). `Extend` scalars keep an
/// open conjunct open; `ZWNJ` is deliberately not in this class, so it ends
/// the conjunct (its `Grapheme_Extend` membership still keeps the grapheme
/// itself unbroken through `GB9`).
fn is_incb_extend(cp: u32) -> bool {
    crate::unicode_tables::incb_extend(cp)
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
    /// Whether the last scalar that is not `InCB=Extend` is an
    /// `InCB=Linker` (virama). This is the left half of UAX #29 `GB9c`
    /// (revision 49, Unicode 18.0.0):
    /// `\p{InCB=Linker} \p{InCB=Extend}* × \p{InCB=Consonant}`.
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
            linker_open: is_incb_linker(prev),
            ri_run: u32::from(is_regional_indicator(prev)),
        }
    }

    /// Advances the run state over one scalar that does not break.
    fn advance(self, cp: u32) -> Self {
        // ZWJ keeps a pictographic run open (`GB11` spans ExtPict
        // (Extend | spacing-mark)* ZWJ); anything else closes it unless it
        // is itself pictographic or a joiner.
        let extend_like = cp == ZWJ || is_grapheme_extend(cp) || is_spacing_mark(cp);
        // `GB9c`: only `InCB=Extend` keeps the linker context open. A
        // second linker re-arms it (a linker is not InCB=Extend); a digit,
        // a spacing mark, `ZWNJ`, or a control closes it.
        let linker_open = if is_incb_extend(cp) {
            self.linker_open
        } else {
            is_incb_linker(cp)
        };
        Self {
            prev: cp,
            prev_hangul: hangul_class(cp),
            pictographic_open: if is_extended_pictographic(cp) {
                true
            } else {
                self.pictographic_open && extend_like
            },
            prev_is_zwj: cp == ZWJ,
            linker_open,
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
    // GB9c: InCB=Linker InCB=Extend* × InCB=Consonant (UAX #29 revision 49,
    // the Unicode 18.0.0 rule shape; the leading-consonant form from earlier
    // revisions no longer applies, and the official fixture pins this one).
    if state.linker_open && is_incb_consonant(current) {
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
        // Spacing mark joins; a consonant + linker + consonant is one
        // conjunct (GB9c).
        assert_eq!(clusters("क\u{093F}"), vec!["क\u{093F}"]);
        assert_eq!(clusters("क\u{094D}ष"), vec!["क\u{094D}ष"]);
        // GB9c requires InCB=Consonant on both sides: a linker followed by
        // a Devanagari digit is not a conjunct.
        assert_eq!(clusters("क\u{094D}\u{0967}"), vec!["क\u{094D}", "\u{0967}"]);
        assert_eq!(clusters("क\u{094D}ष\u{094D}ट"), vec!["क\u{094D}ष\u{094D}ट"]);
        // InCB=Extend scalars (a combining mark) keep the conjunct open.
        assert_eq!(clusters("क\u{0301}\u{094D}ष"), vec!["क\u{0301}\u{094D}ष"]);
        // A linker with no consonant before it still joins the following
        // consonant: the Unicode 18.0.0 rule is `Linker Extend* ×
        // Consonant` (UAX #29 revision 49), pinned by the official fixture.
        assert_eq!(clusters("\u{094D}ष"), vec!["\u{094D}ष"]);
        assert_eq!(clusters("a\u{094D}त"), vec!["a\u{094D}त"]);
        // A non-linker never opens a conjunct: a nukta before the virama is
        // InCB=Extend, so the virama stays the left context...
        assert_eq!(clusters("क\u{093C}\u{094D}ष"), vec!["क\u{093C}\u{094D}ष"]);
        // ...but ZWNJ is not `InCB=Extend`, so it ends the conjunct.
        assert_eq!(
            clusters("क\u{094D}\u{200C}ष"),
            vec!["क\u{094D}\u{200C}", "ष"]
        );
    }

    #[test]
    fn generated_spacing_marks_join_their_base() {
        // Every one of these pairs was outside the historical curated
        // subset and split before the generated table landed.
        for (base, mark) in [
            ('\u{0995}', '\u{09BF}'), // Bengali KA + vowel sign I
            ('\u{07CA}', '\u{07F3}'), // NKo letter + combining dot above (Extend)
            ('\u{0628}', '\u{0653}'), // Arabic BEH + maddah
            ('\u{0F40}', '\u{0F3E}'), // Tibetan KA + sign yar tshes (SpacingMark)
            ('\u{1900}', '\u{1923}'), // Limbu letter + vowel sign EE
            ('\u{1A00}', '\u{1A55}'), // Buginese letter + medial RA
            ('\u{0E01}', '\u{0E33}'), // Thai KO KAI + SARA AM
        ] {
            let text = format!("{base}{mark}");
            assert_eq!(
                clusters(&text),
                vec![text.as_str()],
                "U+{:04X} must join U+{:04X}",
                base as u32,
                mark as u32
            );
        }
    }

    #[test]
    fn prepend_joins_forward_and_takes_no_cell() {
        // Arabic number sign (Prepend) attaches to the following digit.
        assert_eq!(clusters("\u{0600}1"), vec!["\u{0600}1"]);
        // The historical width gap: the prepend must not own a cell, or the
        // base it joins to is hidden behind a spacer.
        assert_eq!(
            cluster_cell_width("\u{08E2}1", AmbiguousWidth::Narrow),
            1,
            "a prepend must not widen the cluster it joins to"
        );
        assert_eq!(
            cluster_cell_width("\u{11A84}\u{0915}", AmbiguousWidth::Narrow),
            1
        );
        // Several prepends still precede one base without breaking.
        assert_eq!(clusters("\u{0600}\u{0601}1"), vec!["\u{0600}\u{0601}1"]);
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
    /// exactly. Rule coverage is the module's full documented set:
    /// GB3-GB9c, GB11-GB13, GB999, over the generated UCD 18.0.0 property
    /// tables. Regenerating the fixture for a newer UCD requires re-running
    /// this test, re-generating the tables, and reviewing the pinned counts
    /// before accepting the bump.
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
