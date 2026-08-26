//! Canonical serialization and the platform-stable state hash
//! (RFC "Deterministic replay guarantees", guarantee 2).
//!
//! The hash is FNV-1a over a canonical byte encoding of every truth-bearing
//! field in a fixed order: little-endian integers, UTF-32 scalars,
//! length-prefixed strings, explicit option tags. No third-party hashing
//! crate participates, so the same input yields the same digest on every
//! platform and toolchain — the property CI asserts byte-for-byte.
//!
//! Serialization version: [`CANONICAL_HASH_VERSION`] is embedded in the
//! stream; its evolution policy follows the RFC open item on the concrete
//! hash serialization version.

use bitty_vt::{Color, UnderlineStyle};

use crate::cell::{Attributes, Cell, Style};

/// Version tag mixed into the canonical stream.
pub const CANONICAL_HASH_VERSION: u32 = 1;

/// Incremental canonical writer backing the state hash.
pub(crate) struct CanonicalHasher {
    state: u64,
}

// 64-bit FNV-1a basis and prime.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl CanonicalHasher {
    pub(crate) fn new() -> Self {
        Self {
            state: FNV_OFFSET_BASIS,
        }
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.state ^= u64::from(value);
        self.state = self.state.wrapping_mul(FNV_PRIME);
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.u8_slice(&value.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.u8_slice(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.u8_slice(&value.to_le_bytes());
    }

    pub(crate) fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    pub(crate) fn option_tag(&mut self, present: bool) {
        self.u8(if present { 1 } else { 0 });
    }

    pub(crate) fn u8_slice(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.u8(b);
        }
    }

    pub(crate) fn char(&mut self, value: char) {
        self.u32(u32::from(value));
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.u64(value.len() as u64);
        self.u8_slice(value.as_bytes());
    }

    pub(crate) fn finish(self) -> u64 {
        self.state
    }
}

pub(crate) fn write_color(out: &mut CanonicalHasher, color: Color) {
    match color {
        Color::Default => out.u8(0),
        Color::Indexed(index) => {
            out.u8(1);
            out.u8(index);
        }
        Color::Rgb(rgb) => {
            out.u8(2);
            out.u8(rgb.r);
            out.u8(rgb.g);
            out.u8(rgb.b);
        }
    }
}

pub(crate) fn write_option_color(out: &mut CanonicalHasher, color: Option<Color>) {
    out.option_tag(color.is_some());
    if let Some(c) = color {
        write_color(out, c);
    }
}

pub(crate) fn write_underline_style(out: &mut CanonicalHasher, style: UnderlineStyle) {
    let discriminant = match style {
        UnderlineStyle::None => 0,
        UnderlineStyle::Single => 1,
        UnderlineStyle::Double => 2,
        UnderlineStyle::Curly => 3,
        UnderlineStyle::Dotted => 4,
        UnderlineStyle::Dashed => 5,
    };
    out.u8(discriminant);
}

pub(crate) fn write_attributes(out: &mut CanonicalHasher, attrs: &Attributes) {
    out.boolean(attrs.bold);
    out.boolean(attrs.faint);
    out.boolean(attrs.italic);
    write_underline_style(out, attrs.underline);
    out.boolean(attrs.blink);
    out.boolean(attrs.inverse);
    out.boolean(attrs.invisible);
    out.boolean(attrs.strikethrough);
}

pub(crate) fn write_style(out: &mut CanonicalHasher, style: &Style) {
    write_option_color(out, style.foreground);
    write_option_color(out, style.background);
    write_option_color(out, style.underline_color);
    write_attributes(out, &style.attributes);
}

pub(crate) fn write_cell(out: &mut CanonicalHasher, cell: &Cell) {
    out.char(cell.glyph);
    write_style(out, &cell.style);
    out.u8(cell.width);
    out.boolean(cell.spacer);
    out.option_tag(cell.hyperlink.is_some());
    if let Some(link) = cell.hyperlink {
        out.u32(link.as_u32());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hasher_is_order_sensitive_and_stable() {
        fn hash(writes: &dyn Fn(&mut CanonicalHasher)) -> u64 {
            let mut h = CanonicalHasher::new();
            writes(&mut h);
            h.finish()
        }
        let abc_then_7 = |h: &mut CanonicalHasher| {
            h.str("abc");
            h.u32(7);
        };
        let seven_then_abc = |h: &mut CanonicalHasher| {
            h.u32(7);
            h.str("abc");
        };
        assert_eq!(
            hash(&abc_then_7),
            hash(&abc_then_7),
            "same writes must hash identically"
        );
        assert_ne!(
            hash(&abc_then_7),
            hash(&seven_then_abc),
            "field order is part of the contract"
        );
    }

    #[test]
    fn version_pin_is_explicit() {
        assert_eq!(CANONICAL_HASH_VERSION, 1);
    }
}
