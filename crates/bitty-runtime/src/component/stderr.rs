//! Bounded stderr capture ring for component processes.

use std::collections::VecDeque;
use std::fmt::Write as _;

use super::COMPONENT_STDERR_LOG_TAIL_BYTES;

/// UTF-8 continuation bytes are `0b10xx_xxxx`.
const UTF8_CONTINUATION_MASK: u8 = 0b1100_0000;
const UTF8_CONTINUATION_TAG: u8 = 0b1000_0000;
/// Longest UTF-8 sequence; at most this many orphaned bytes are skipped.
const UTF8_MAX_SEQUENCE_BYTES: usize = 4;

/// The newest [`COMPONENT_STDERR_LOG_TAIL_BYTES`] of `ring` as one log-safe
/// line (DIR-030 D4).
///
/// Bytes are decoded as UTF-8 (invalid sequences become U+FFFD; a sequence
/// cut by the tail boundary is dropped). Backslash and every control or
/// invisible formatting character (C0, DEL, C1, zero-width, and bidi
/// controls) are escaped (`\n`, `\r`, `\t`, `\\`, otherwise `\u{..}`), so the
/// output can never inject terminal sequences or fake log lines.
#[must_use]
pub fn stderr_log_tail(ring: &[u8]) -> String {
    let mut tail = &ring[ring.len().saturating_sub(COMPONENT_STDERR_LOG_TAIL_BYTES)..];
    if tail.len() < ring.len() {
        let orphans = tail
            .iter()
            .take(UTF8_MAX_SEQUENCE_BYTES - 1)
            .take_while(|byte| *byte & UTF8_CONTINUATION_MASK == UTF8_CONTINUATION_TAG)
            .count();
        tail = &tail[orphans..];
    }
    let text = String::from_utf8_lossy(tail);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if needs_escape(c) => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out
}

/// Controls plus invisible format characters that can reorder or hide text.
fn needs_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// Fixed-capacity byte ring keeping the newest bytes a component wrote to
/// stderr. Older bytes are dropped first; memory never exceeds the
/// capacity.
#[derive(Debug, Clone)]
pub struct StderrRing {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: u64,
}

impl StderrRing {
    /// Empty ring holding at most `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    /// Append `chunk`, evicting the oldest bytes beyond the capacity.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.capacity == 0 {
            self.dropped = self.dropped.saturating_add(chunk.len() as u64);
            return;
        }
        let tail = if chunk.len() > self.capacity {
            let skip = chunk.len() - self.capacity;
            self.dropped = self.dropped.saturating_add(skip as u64);
            &chunk[skip..]
        } else {
            chunk
        };
        let overflow = (self.bytes.len() + tail.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.dropped = self.dropped.saturating_add(overflow as u64);
        }
        self.bytes.extend(tail.iter().copied());
    }

    /// Current contents, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    /// Bytes currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the ring holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Total bytes evicted or skipped since creation.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Configured capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_escapes_controls_and_bounds_length() {
        assert_eq!(stderr_log_tail(b""), "");
        assert_eq!(
            stderr_log_tail(b"ok \x1b[31mred\x07\r\n\tback\\slash\x7f"),
            "ok \\u{1b}[31mred\\u{7}\\r\\n\\tback\\\\slash\\u{7f}"
        );
        // C1 controls and bidi/zero-width format characters are escaped too.
        assert_eq!(
            stderr_log_tail("a\u{9b}b\u{202e}c\u{200b}d\u{2066}e".as_bytes()),
            "a\\u{9b}b\\u{202e}c\\u{200b}d\\u{2066}e"
        );
        // Invalid UTF-8 is replaced, never passed through raw.
        assert_eq!(stderr_log_tail(b"x\xffy"), "x\u{fffd}y");
        // Only the newest COMPONENT_STDERR_LOG_TAIL_BYTES are kept.
        let mut long = vec![b'a'; COMPONENT_STDERR_LOG_TAIL_BYTES];
        long.extend_from_slice(b"END");
        let tail = stderr_log_tail(&long);
        assert_eq!(tail.len(), COMPONENT_STDERR_LOG_TAIL_BYTES);
        assert!(tail.ends_with("END"));
        // A cut through a multi-byte character drops the orphaned bytes.
        let mut cut = "a€".as_bytes().to_vec(); // 1 + 3 bytes
        cut.extend(vec![b'b'; COMPONENT_STDERR_LOG_TAIL_BYTES - 2]);
        let tail = stderr_log_tail(&cut);
        assert!(!tail.contains('\u{fffd}'), "{tail:?}");
        assert_eq!(tail, "b".repeat(COMPONENT_STDERR_LOG_TAIL_BYTES - 2));
    }

    #[test]
    fn keeps_newest_bytes_within_capacity() {
        let mut ring = StderrRing::new(4);
        ring.push(b"ab");
        ring.push(b"cdef");
        assert_eq!(ring.snapshot(), b"cdef");
        assert_eq!(ring.dropped(), 2);
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.dropped(), 12);
    }

    #[test]
    fn zero_capacity_holds_nothing() {
        let mut ring = StderrRing::new(0);
        ring.push(b"abc");
        assert!(ring.is_empty());
        assert_eq!(ring.dropped(), 3);
    }

    #[test]
    fn default_bound_is_64_kib() {
        let mut ring = StderrRing::new(super::super::COMPONENT_STDERR_MAX_BYTES);
        ring.push(&vec![b'x'; 200 * 1024]);
        assert_eq!(ring.len(), 64 * 1024);
        assert_eq!(ring.capacity(), 64 * 1024);
    }
}
