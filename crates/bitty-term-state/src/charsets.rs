//! Charset designation and invocation (SCS, SO/SI, single shifts).
//!
//! The parser delivers fully resolved [`bitty_vt::SelectCharset`] and
//! [`bitty_vt::InvokeCharset`] actions; terminal state owns the four
//! G0-G3 slots, the active locking shift, and any armed single shift.
//! Translation applies to printed scalars only (RFC "Typed Action
//! interface": parameters arrive resolved; state interprets).
//!
//! Documented invocation rule: designating or invoking `G0`/`G1` selects
//! the locking shift (`SO`/`SI` semantics); invoking `G2`/`G3` arms a
//! single shift consumed by exactly one subsequent print (`SS2`/`SS3`
//! semantics).

use bitty_vt::{CharsetSlot, CharsetTable};

/// The four designated tables plus shift state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Charsets {
    /// Designated table per slot, indexed by
    /// [`bitty_vt::CharsetSlot`] discriminant order (G0..G3).
    pub slots: [CharsetTable; 4],
    /// Slot currently invoked by locking shift for normal printing.
    pub locking: CharsetSlot,
    /// Armed single shift, consumed by the next print.
    pub single: Option<CharsetSlot>,
}

impl Default for Charsets {
    fn default() -> Self {
        Self {
            slots: [CharsetTable::Ascii; 4],
            locking: CharsetSlot::G0,
            single: None,
        }
    }
}

impl Charsets {
    /// Records a designation into a slot.
    pub fn designate(&mut self, slot: CharsetSlot, table: CharsetTable) {
        self.slots[slot_index(slot)] = table;
    }

    /// Applies an invocation per the documented rule above.
    pub fn invoke(&mut self, slot: CharsetSlot) {
        match slot {
            CharsetSlot::G0 | CharsetSlot::G1 => self.locking = slot,
            CharsetSlot::G2 | CharsetSlot::G3 => self.single = Some(slot),
        }
    }

    /// Resolves and consumes the effective table for one printed scalar.
    #[must_use]
    pub fn consume_translation_table(&mut self) -> CharsetTable {
        let slot = self.single.take().unwrap_or(self.locking);
        self.slots[slot_index(slot)]
    }

    /// Translates one scalar through `table`.
    #[must_use]
    pub fn translate(table: CharsetTable, c: char) -> char {
        match table {
            CharsetTable::Ascii => c,
            CharsetTable::UnitedKingdom => {
                if c == '#' {
                    '\u{00A3}'
                } else {
                    c
                }
            }
            // DEC Special Graphics per the VT100 user guide; unmapped
            // scalars pass through unchanged.
            CharsetTable::DecSpecialGraphics => dec_special_graphics(c),
        }
    }
}

#[must_use]
fn slot_index(slot: CharsetSlot) -> usize {
    match slot {
        CharsetSlot::G0 => 0,
        CharsetSlot::G1 => 1,
        CharsetSlot::G2 => 2,
        CharsetSlot::G3 => 3,
    }
}

/// DEC Special Graphics translation for the `0x5f..=0x7e` range.
#[must_use]
fn dec_special_graphics(c: char) -> char {
    match c {
        '_' => ' ',
        '`' => '\u{25C6}',
        'a' => '\u{2592}',
        'b' => '\u{2409}',
        'c' => '\u{240C}',
        'd' => '\u{240D}',
        'e' => '\u{240A}',
        'f' => '\u{00B0}',
        'g' => '\u{00B1}',
        'h' => '\u{2420}',
        'i' => '\u{240B}',
        'j' => '\u{2518}',
        'k' => '\u{2510}',
        'l' => '\u{250C}',
        'm' => '\u{2514}',
        'n' => '\u{253C}',
        'o' => '\u{23BA}',
        'p' => '\u{23BB}',
        'q' => '\u{2500}',
        'r' => '\u{23BC}',
        's' => '\u{23BD}',
        't' => '\u{251C}',
        'u' => '\u{2524}',
        'v' => '\u{2534}',
        'w' => '\u{252C}',
        'x' => '\u{2502}',
        'y' => '\u{2264}',
        'z' => '\u{2265}',
        '{' => '\u{03C0}',
        '|' => '\u{2260}',
        '}' => '\u{00A3}',
        '~' => '\u{00B7}',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn designation_and_locking_shift() {
        let mut cs = Charsets::default();
        cs.designate(CharsetSlot::G1, CharsetTable::DecSpecialGraphics);
        cs.invoke(CharsetSlot::G1);
        assert_eq!(cs.locking, CharsetSlot::G1);
        assert_eq!(
            cs.consume_translation_table(),
            CharsetTable::DecSpecialGraphics
        );
        assert_eq!(cs.single, None);
    }

    #[test]
    fn single_shift_is_consumed_once() {
        let mut cs = Charsets::default();
        cs.designate(CharsetSlot::G2, CharsetTable::UnitedKingdom);
        cs.invoke(CharsetSlot::G2);
        assert_eq!(cs.single, Some(CharsetSlot::G2));
        assert_eq!(cs.consume_translation_table(), CharsetTable::UnitedKingdom);
        assert_eq!(cs.single, None);
        assert_eq!(cs.consume_translation_table(), CharsetTable::Ascii);
    }

    #[test]
    fn dec_special_graphics_maps_line_drawing() {
        assert_eq!(
            Charsets::translate(CharsetTable::DecSpecialGraphics, 'q'),
            '\u{2500}'
        );
        assert_eq!(
            Charsets::translate(CharsetTable::DecSpecialGraphics, 'x'),
            '\u{2502}'
        );
        assert_eq!(
            Charsets::translate(CharsetTable::DecSpecialGraphics, 'A'),
            'A'
        );
    }

    #[test]
    fn united_kingdom_maps_pound_sign() {
        assert_eq!(
            Charsets::translate(CharsetTable::UnitedKingdom, '#'),
            '\u{00A3}'
        );
        assert_eq!(Charsets::translate(CharsetTable::UnitedKingdom, 'a'), 'a');
    }
}
