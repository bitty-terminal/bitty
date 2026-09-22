//! UAX #29 grapheme-cluster segmentation over terminal text (M1, #1146).
//!
//! Candidate implementation of the cluster-segmentation half of the text
//! domain: where [`crate::char_cell_width`] answers "how many cells does
//! this scalar occupy", this module answers "which scalars travel together".
//! The segmenter is dependency-free, deterministic, and total over any
//! `&str`: no I/O, no allocation beyond the returned slices, no platform
//! variance. Lookup tables are curated M1 subsets of the Unicode Character
//! Database, documented below; unlisted scalars resolve to the fail-open
//! `GB999 Any ÷ Any` break, never to a panic or an over-join.
//!
//! Rule coverage (UAX #29, grapheme-cluster boundary rules):
//!
//! - `GB3` (`CR × LF`), `GB4`/`GB5` (break around controls): full.
//! - `GB6`–`GB8` (Hangul syllables): Jamo `1100..11FF`, extended Jamo
//!   `A960..A97C`/`D7B0..D7FB`, and precomposed syllables `AC00..D7A3`.
//! - `GB9` (`× Extend | ZWJ`): [`is_grapheme_extend`] over the crate
//!   zero-width tables plus emoji modifiers `1F3FB..1F3FF`; format controls
//!   (bidi marks, Arabic format controls, `FEFF`, tags `E0000..E0001`)
//!   break instead of joining, matching their `Control` grapheme property.
//! - `GB9a` (`× SpacingMark`): curated Indic, Thai/Lao, Myanmar, and Khmer
//!   spacing-mark ranges. Unlisted spacing marks break (table gap).
//! - `GB9b` (`× Prepend`): not implemented; prepend format controls break.
//!   Prepend scalars are vanishingly rare in terminal text, and joining
//!   them forward would mis-measure cursor runs for every other consumer.
//! - `GB11` (extended-pictographic ZWJ sequences): full over the curated
//!   [`is_extended_pictographic`] set (the width emoji-presentation ranges
//!   plus `1F300..1F64F`/`1F680..1F6FF`/`1F900..1F9FF`).
//! - `GB12`/`GB13` (regional-indicator pairing): full, with run parity.
//! - Indic conjuncts (post-15.0 `GB9c`, virama linkers): M1-lite rule — no
//!   break between a virama linker and a following Indic/Tibetan/Myanmar/
//!   Khmer letter. The full `InCB` property table follows the text RFC.
//!
//! Tibetan subjoining beyond the combining vowel-sign range `0F72..0F84`
//! segments conservatively (breaks where a full `InCB` table would join);
//! that is a table gap, stated here so no reviewer mistakes it for shaping.
//!
//! [`Graphemes`] yields the clusters of a string in order; [`split_clusters`]
//! collects them. [`cluster_cell_width`] measures one cluster for grid work:
//! the maximum scalar width under the given [`AmbiguousWidth`] policy (`0`
//! for the empty cluster). Regional-indicator pairs therefore measure `1`
//! per cluster while occupying two grid cells as two width-1 scalars —
//! consistent with how the state print path stores them.

use super::cell::{AmbiguousWidth, char_cell_width_in, is_zero_width};

/// Zero-width joiner: joins backward (`GB9`), and forward only across an
/// extended-pictographic run (`GB11`).
const ZWJ: u32 = 0x200D;

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
        | 0x0600..=0x0605
        | 0x061C
        | 0x06DD
        | 0x070F
        | 0x0890..=0x0891
        | 0x200B
        | 0x200E..=0x200F
        | 0x2028..=0x202E
        | 0x2060..=0x2064
        | 0xFEFF
        | 0xE0000..=0xE0001
    )
}

/// Whether `cp` is `Grapheme_Extend` in the M1 curated table (`GB9`).
///
/// The crate zero-width tables minus the format controls (which break, see
/// [`is_break_control`]) and minus ZWJ (own class), plus emoji modifiers
/// `1F3FB..1F3FF` (wide scalars that still join forward).
#[must_use]
pub fn is_grapheme_extend(cp: u32) -> bool {
    if cp == ZWJ || is_break_control(cp) {
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
/// `0E33`, Lao `0EB3`/`0EBD`, Myanmar `102B..1032`/`1036..1038`, Khmer
/// `17B6`/`17BE..17C5`/`17C7..17C8`. Unlisted spacing marks break.
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
        | 0x102B..=0x1032
        | 0x1036..=0x1038
        | 0x17B6
        | 0x17BE..=0x17C5
        | 0x17C7..=0x17C8
    )
}

/// Whether `cp` may form an emoji ZWJ sequence (`GB11`).
///
/// The width emoji-presentation ranges plus the pictographic blocks
/// `1F300..1F64F`, `1F680..1F6FF`, and `1F900..1F9FF`.
#[must_use]
pub fn is_extended_pictographic(cp: u32) -> bool {
    matches!(cp,
        0x231A..=0x231B
        | 0x23E9..=0x23EC
        | 0x23F0
        | 0x23F3
        | 0x25FD..=0x25FE
        | 0x2614..=0x2615
        | 0x2648..=0x2653
        | 0x267F
        | 0x2693
        | 0x26A1
        | 0x26AA..=0x26AB
        | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5
        | 0x26CE
        | 0x26D4
        | 0x26EA
        | 0x26F2..=0x26F3
        | 0x26F5
        | 0x26FA
        | 0x26FD
        | 0x2705
        | 0x270A..=0x270B
        | 0x2728
        | 0x274C
        | 0x274E
        | 0x2753..=0x2755
        | 0x2757
        | 0x2795..=0x2797
        | 0x27B0
        | 0x27BF
        | 0x2B1B..=0x2B1C
        | 0x2B50
        | 0x2B55
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F900..=0x1F9FF
    )
}

/// Whether `cp` is a regional indicator (`GB12`/`GB13`).
#[must_use]
pub fn is_regional_indicator(cp: u32) -> bool {
    matches!(cp, 0x1F1E6..=0x1F1FF)
}

/// Virama-style linkers for the M1-lite conjunct rule (`GB9c` approximation).
///
/// Devanagari `094D`, Bengali `09CD`, Gurmukhi `0A4D`, Gujarati `0ACD`,
/// Oriya `0B4D`, Tamil `0BCD`, Telugu `0C4D`, Kannada `0CCD`, Malayalam
/// `0D4D`, Sinhala `0DCA`, Myanmar `1039`, Khmer `17D2`. A linker never
/// ends a cluster before a following Indic-block letter.
fn is_linker(cp: u32) -> bool {
    matches!(
        cp,
        0x094D
            | 0x09CD
            | 0x0A4D
            | 0x0ACD
            | 0x0B4D
            | 0x0BCD
            | 0x0C4D
            | 0x0CCD
            | 0x0D4D
            | 0x0DCA
            | 0x1039
            | 0x17D2
    )
}

/// Letters the linker rule may join onto (Indic blocks, Tibetan, Myanmar,
/// Khmer): any scalar in these ranges that is not itself a joiner,
/// control, or indicator.
fn is_linker_target(cp: u32) -> bool {
    if is_grapheme_extend(cp)
        || is_spacing_mark(cp)
        || is_break_control(cp)
        || is_regional_indicator(cp)
        || cp == ZWJ
    {
        return false;
    }
    matches!(cp,
        0x0900..=0x0DFF | 0x0F00..=0x0FFF | 0x1000..=0x109F | 0x1780..=0x17FF
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
            ri_run: u32::from(is_regional_indicator(prev)),
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
    // GB11: ExtPict Extend* ZWJ × ExtPict.
    if state.prev_is_zwj && state.pictographic_open && is_extended_pictographic(current) {
        return false;
    }
    // GB12 / GB13: keep regional indicators paired.
    if is_regional_indicator(prev) && is_regional_indicator(current) {
        return state.ri_run % 2 == 0;
    }
    // M1-lite GB9c: linker × Indic-block letter.
    if is_linker(prev) && is_linker_target(current) {
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
            let hangul = hangul_class(cp);
            // ZWJ keeps a pictographic run open (`GB11` spans ExtPict
            // (Extend | spacing-mark)* ZWJ); anything else closes it unless
            // it is itself pictographic or a joiner.
            let extend_like = cp == ZWJ || is_grapheme_extend(cp) || is_spacing_mark(cp);
            state = RunState {
                prev: cp,
                prev_hangul: hangul,
                pictographic_open: if is_extended_pictographic(cp) {
                    true
                } else {
                    state.pictographic_open && extend_like
                },
                prev_is_zwj: cp == ZWJ,
                ri_run: if is_regional_indicator(cp) {
                    state.ri_run + 1
                } else {
                    0
                },
            };
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
/// The maximum scalar width in the cluster (`0` for the empty cluster), so
/// a base plus its combining marks measures as the base, and a ZWJ emoji
/// sequence measures as its wide lead. Regional-indicator pairs measure `1`
/// per cluster while occupying two width-1 grid cells; see the module docs.
#[must_use]
pub fn cluster_cell_width(cluster: &str, ambiguous: AmbiguousWidth) -> u8 {
    cluster
        .chars()
        .map(|c| char_cell_width_in(c, ambiguous))
        .max()
        .unwrap_or(0)
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
    fn cluster_width_is_max_scalar_width() {
        assert_eq!(cluster_cell_width("", AmbiguousWidth::Narrow), 0);
        assert_eq!(cluster_cell_width("a", AmbiguousWidth::Narrow), 1);
        assert_eq!(cluster_cell_width("中", AmbiguousWidth::Narrow), 2);
        assert_eq!(cluster_cell_width("e\u{0301}", AmbiguousWidth::Narrow), 1);
        let family = "👩\u{200D}👩\u{200D}👧";
        assert_eq!(cluster_cell_width(family, AmbiguousWidth::Narrow), 2);
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
}
