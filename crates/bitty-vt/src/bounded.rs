//! Length-bounded payload types.
//!
//! The parser obligations in the Terminal State RFC require hard payload
//! limits so that oversized OSC payloads yield well-defined truncation
//! instead of unbounded growth. These types make the bound part of the type
//! invariant: a `BoundedString` or `BoundedBytes` can never exceed
//! [`BoundedString::MAX_LEN`] / [`BoundedBytes::MAX_LEN`] bytes.
//!
//! Truncation is deterministic: input longer than the cap is cut at exactly
//! the cap (at the nearest lower UTF-8 character boundary for strings), so
//! the same byte stream always produces the same truncated value.

/// A UTF-8 string capped at [`BoundedString::MAX_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoundedString(Box<str>);

/// Raw bytes capped at [`BoundedBytes::MAX_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoundedBytes(Box<[u8]>);

impl BoundedString {
    /// Hard cap in bytes for any string payload carried by an action.
    pub const MAX_LEN: usize = 4096;

    /// Wraps `value`, deterministically truncating it to `MAX_LEN` bytes.
    ///
    /// Truncation never splits a UTF-8 character: the result is the longest
    /// prefix of complete characters that fits within the cap.
    #[must_use]
    pub fn new(value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        if value.len() <= Self::MAX_LEN {
            return Self(Box::from(value));
        }
        let mut end = Self::MAX_LEN;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        Self(Box::from(&value[..end]))
    }

    /// The contained string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<str> for BoundedString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for BoundedString {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for BoundedString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl BoundedBytes {
    /// Hard cap in bytes for any binary payload carried by an action.
    pub const MAX_LEN: usize = 4096;

    /// Wraps `value`, deterministically truncating it to `MAX_LEN` bytes.
    #[must_use]
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        let mut value = value.into();
        value.truncate(Self::MAX_LEN);
        Self(value.into_boxed_slice())
    }

    /// The contained bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl AsRef<[u8]> for BoundedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for BoundedBytes {
    fn from(value: &[u8]) -> Self {
        Self::new(value.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for BoundedBytes {
    fn from(value: &[u8; N]) -> Self {
        Self::new(value.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_within_cap_is_unchanged() {
        let s = BoundedString::new("hello");
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn string_truncates_at_cap_on_char_boundary() {
        let four_byte_char = "\u{1F600}";
        // 2048 copies of a 4-byte char: far beyond the cap.
        let long: String = four_byte_char.repeat(2048);
        let bounded = BoundedString::new(long);
        assert_eq!(bounded.len(), BoundedString::MAX_LEN);
        assert_eq!(bounded.len() % 4, 0);
        assert!(bounded.as_str().chars().all(|c| c == '\u{1F600}'));
    }

    #[test]
    fn string_deterministic_truncation() {
        let long = "x".repeat(BoundedString::MAX_LEN + 100);
        assert_eq!(
            BoundedString::new(long.clone()).as_str(),
            BoundedString::new(long).as_str()
        );
    }

    #[test]
    fn bytes_truncate_at_cap() {
        let long = vec![0xAB_u8; BoundedBytes::MAX_LEN + 10];
        let bounded = BoundedBytes::new(long);
        assert_eq!(bounded.len(), BoundedBytes::MAX_LEN);
        assert!(bounded.as_bytes().iter().all(|&b| b == 0xAB));
    }
}
