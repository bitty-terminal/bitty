//! The terminal content plane: bounded byte buffering, frame budgets, and the
//! binary-safe hex codec (contract `D-10`, `D-14`; bitty#2790).
//!
//! The daemon never interprets terminal bytes. A terminal session's PTY output
//! is drained into one [`ContentBuffer`] per session, and that buffer is the
//! only place the daemon stores content. Two readers share it:
//!
//! - `session.read` pulls bytes synchronously (bounded by
//!   [`CONTENT_FRAME_BYTES`]);
//! - `session.subscribe` pushes [`ContentFrame`]s on the negotiated event
//!   connection (each frame is bounded by [`CONTENT_FRAME_BYTES`]; one delivery
//!   pass drains as many whole frames as the connection's reserved outbound
//!   room allows).
//!
//! Consumption is single-owner: whichever path takes bytes first removes them,
//! so a client uses a pull loop or a subscription, never both expecting the
//! same bytes. The buffer is capped at [`CONTENT_BUFFER_BYTES`]; when it is
//! full the PTY is no longer drained and the kernel PTY buffer provides
//! backpressure to the child. A slow reader therefore cannot grow memory or
//! block the single-threaded service loop.
//!
//! # Frame budget
//!
//! The transport `max_chunk_bytes` and `max_frame_bytes` are both 256 KiB.
//! Terminal bytes are rendered as lowercase hex, so one raw byte costs two
//! JSON string bytes. [`CONTENT_FRAME_BYTES`] is one quarter of
//! `max_chunk_bytes`, which leaves ample room for the envelope under the frame
//! bound. [`CONTENT_FRAME_ENCODED_MAX`] is the conservative wire-size ceiling
//! the server reserves before it takes a frame from the buffer, so a frame is
//! either queued whole or left in the buffer for a later pass.
//!
//! # What a frame never carries
//!
//! No pid, executable path, cwd, environment, argv, token, or audit text.
//! Content frames carry the frozen event envelope fact (`instance_id`,
//! `revision`, and a constant `terminal: false`) plus the session id,
//! generation, sequence, raw bytes, and the current buffered-byte count.
//!
//! # Untrusted observation
//!
//! Every response and frame that carries terminal bytes is labeled with
//! [`UntrustedObservation`] (`is_untrusted_surface: true`), the `bitty-ipc`
//! vocabulary for terminal output (T-10 / R-013). The service attaches the
//! label after the handler returns, so a request cannot suppress or forge it,
//! and a response that clips or rewrites bytes still carries it.

use std::collections::VecDeque;
use std::fmt;

use serde_json::Value;

use crate::limits::LimitId;
use crate::registry::{AttachId, DaemonInstanceId, SessionId};

/// Raw terminal bytes one content frame may carry.
///
/// One quarter of the inherited `max_chunk_bytes`; see the module docs for the
/// hex-encoding budget.
pub const CONTENT_FRAME_BYTES: usize = (LimitId::MaxChunkBytes.default_value() as usize) / 4;

/// Conservative encoded size of one content frame.
///
/// Two hex characters per raw byte plus a fixed envelope allowance. The server
/// reserves this much outbound room before taking a frame, so a frame that is
/// handed to the transport always fits the connection bound.
pub const CONTENT_FRAME_ENCODED_MAX: usize = CONTENT_FRAME_BYTES * 2 + 1_024;

/// Bytes one session may retain for content reads and frame delivery.
///
/// One inherited chunk. Once full, the PTY is left undrained for that session
/// until a reader consumes bytes; the child sees normal terminal
/// backpressure.
pub const CONTENT_BUFFER_BYTES: usize = LimitId::MaxChunkBytes.default_value() as usize;

/// The frozen event kind for a pushed content frame.
pub const CONTENT_EVENT_KIND: &str = "session_content";

/// The server-attached untrusted-observation label (`T-10` / `R-013`).
///
/// Terminal bytes are observation data, never instructions. The label uses the
/// `bitty-ipc` result vocabulary exactly: the boolean field
/// [`Self::FIELD`] (`is_untrusted_surface`), always `true`. The type has one
/// value, so a response cannot be built with a false or absent label, and the
/// service attaches it after the handler returns: a request field can neither
/// suppress nor forge it. Redaction, truncation, or any other rewrite of the
/// response body happens before attachment and cannot remove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UntrustedObservation;

impl UntrustedObservation {
    /// The wire field name, identical to the `bitty-ipc` result field.
    pub const FIELD: &'static str = "is_untrusted_surface";

    /// The one label value.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Whether the labeled surface is an untrusted observation.
    ///
    /// Always `true`; provided so call sites read intent, not a field.
    #[must_use]
    pub const fn is_untrusted_surface(self) -> bool {
        true
    }

    /// Attach the label to a response object.
    ///
    /// Returns `true` when the label was inserted. A non-object response
    /// returns `false` and is left untouched, so a caller that expects a
    /// content response treats it as an internal invariant rather than
    /// serving unlabeled bytes.
    pub fn attach(self, response: &mut Value) -> bool {
        match response {
            Value::Object(object) => {
                object.insert(
                    Self::FIELD.to_owned(),
                    Value::Bool(self.is_untrusted_surface()),
                );
                true
            }
            _ => false,
        }
    }
}

/// One bounded per-session content buffer.
///
/// The buffer owns raw bytes only. It never exposes a pid, path, or descriptor,
/// and its [`fmt::Debug`] prints lengths rather than content.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentBuffer {
    bytes: VecDeque<u8>,
    capacity: usize,
}

impl ContentBuffer {
    /// An empty buffer with the compiled per-session cap.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(CONTENT_BUFFER_BYTES)
    }

    /// An empty buffer with an explicit cap (unit tests and adapters).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            capacity,
        }
    }

    /// Bytes currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Bytes that can still be appended before the cap.
    #[must_use]
    pub fn remaining_capacity(&self) -> usize {
        self.capacity.saturating_sub(self.bytes.len())
    }

    /// Whether the buffer is at its cap.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.remaining_capacity() == 0
    }

    /// Append `data`, accepting only the bytes that fit.
    ///
    /// Returns the accepted count. The caller must leave the remainder in the
    /// PTY (or retry later); this method never grows past the cap.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let accepted = data.len().min(self.remaining_capacity());
        self.bytes.extend(&data[..accepted]);
        accepted
    }

    /// Take up to `max_bytes` from the front.
    ///
    /// Returns an empty vector for a zero bound or an empty buffer.
    #[must_use]
    pub fn pop(&mut self, max_bytes: usize) -> Vec<u8> {
        let take = max_bytes.min(self.bytes.len());
        self.bytes.drain(..take).collect()
    }
}

impl Default for ContentBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ContentBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContentBuffer")
            .field("bytes", &self.bytes.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

/// One pushed content frame, ready for the wire renderer.
///
/// The frame carries the frozen event envelope facts so it can share the
/// negotiated event connection with the metadata events (`D-10`): the daemon
/// instance, the session's `state_revision` at emission, and a constant
/// `terminal: false`. Content ordering remains the per-subscription
/// `sequence`, because taking bytes from the buffer does not advance the
/// snapshot revision.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentFrame {
    /// The daemon instance that emitted the frame.
    pub instance_id: DaemonInstanceId,
    /// The session the bytes belong to.
    pub session_id: SessionId,
    /// The controller lease that owns the subscription.
    pub attach_id: AttachId,
    /// The fenced worker generation.
    pub generation: u64,
    /// The session's `state_revision` when the frame was taken.
    pub revision: u64,
    /// Per-subscription frame sequence, starting at zero.
    pub sequence: u64,
    /// Raw terminal bytes, never interpreted by the daemon.
    pub data: Vec<u8>,
    /// Bytes still buffered after this frame was taken.
    pub buffered_bytes: usize,
}

impl fmt::Debug for ContentFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContentFrame")
            .field("instance_id", &self.instance_id)
            .field("session_id", &self.session_id)
            .field("attach_id", &self.attach_id)
            .field("generation", &self.generation)
            .field("revision", &self.revision)
            .field("sequence", &self.sequence)
            .field("data_bytes", &self.data.len())
            .field("buffered_bytes", &self.buffered_bytes)
            .finish()
    }
}

/// Why a hex-encoded content field was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexError {
    /// The string is empty, odd-length, or contains a non-hex character.
    Invalid,
    /// The decoded length exceeds the supplied bound.
    Limit {
        /// The enforced raw-byte bound.
        limit: usize,
        /// The decoded length.
        actual: usize,
    },
}

impl fmt::Display for HexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => formatter.write_str("content is not lowercase hexadecimal"),
            Self::Limit { limit, actual } => {
                write!(
                    formatter,
                    "content is {actual} bytes, above the {limit}-byte bound"
                )
            }
        }
    }
}

impl std::error::Error for HexError {}

/// Encode bytes as lowercase hexadecimal.
#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Decode lowercase hexadecimal with a raw-byte bound.
///
/// Uppercase hex, whitespace, a prefix, and an odd length are all
/// [`HexError::Invalid`]. The decoder checks the bound before allocating the
/// decoded vector.
///
/// # Errors
///
/// [`HexError::Invalid`] for a malformed value and [`HexError::Limit`] when
/// the decoded length is over `max_bytes`.
pub fn decode_hex(text: &str, max_bytes: usize) -> Result<Vec<u8>, HexError> {
    if text.is_empty() || text.len() % 2 != 0 {
        return Err(HexError::Invalid);
    }
    let decoded_len = text.len() / 2;
    if decoded_len > max_bytes {
        return Err(HexError::Limit {
            limit: max_bytes,
            actual: decoded_len,
        });
    }
    let mut decoded = Vec::with_capacity(decoded_len);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_nibble(bytes[index]).ok_or(HexError::Invalid)?;
        let low = hex_nibble(bytes[index + 1]).ok_or(HexError::Invalid)?;
        decoded.push((high << 4) | low);
        index += 2;
    }
    Ok(decoded)
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_every_byte_value() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let encoded = encode_hex(&bytes);
        assert_eq!(encoded.len(), bytes.len() * 2);
        assert_eq!(decode_hex(&encoded, bytes.len()).unwrap(), bytes);
    }

    #[test]
    fn hex_rejects_malformed_and_uppercase_input() {
        assert_eq!(decode_hex("", 16), Err(HexError::Invalid));
        assert_eq!(decode_hex("0", 16), Err(HexError::Invalid));
        assert_eq!(decode_hex("0g", 16), Err(HexError::Invalid));
        assert_eq!(decode_hex("AB", 16), Err(HexError::Invalid));
        assert_eq!(decode_hex(" 00", 16), Err(HexError::Invalid));
    }

    #[test]
    fn hex_checks_the_bound_before_decoding() {
        assert_eq!(
            decode_hex("0011", 1),
            Err(HexError::Limit {
                limit: 1,
                actual: 2
            })
        );
        assert_eq!(decode_hex("0011", 2).unwrap(), vec![0, 0x11]);
    }

    #[test]
    fn a_buffer_never_grows_past_its_cap() {
        let mut buffer = ContentBuffer::with_capacity(4);
        assert_eq!(buffer.push(b"abc"), 3);
        assert_eq!(buffer.push(b"def"), 1);
        assert_eq!(buffer.len(), 4);
        assert!(buffer.is_full());
        assert_eq!(buffer.push(b"x"), 0);
        assert_eq!(buffer.pop(2), b"ab");
        assert_eq!(buffer.remaining_capacity(), 2);
        assert_eq!(buffer.pop(100), b"cd");
        assert!(buffer.is_empty());
        assert_eq!(buffer.pop(1), Vec::<u8>::new());
    }

    #[test]
    fn frame_budget_is_inside_the_transport_bounds() {
        let chunk = LimitId::MaxChunkBytes.default_value() as usize;
        let frame = LimitId::MaxFrameBytes.default_value() as usize;
        assert_eq!(CONTENT_FRAME_BYTES * 4, chunk);
        assert!(CONTENT_FRAME_ENCODED_MAX < frame);
        assert_eq!(CONTENT_BUFFER_BYTES, chunk);
    }

    #[test]
    fn the_untrusted_label_is_always_true_and_survives_response_rewrites() {
        let label = UntrustedObservation::new();
        assert!(label.is_untrusted_surface());

        let mut response = serde_json::json!({"data": "0011", "truncated": false});
        assert!(label.attach(&mut response));
        assert_eq!(response[UntrustedObservation::FIELD], true);
        assert!(response[UntrustedObservation::FIELD].is_boolean());

        // A redaction or truncation rewrites the body; the label is attached
        // after every handler transformation and stays present.
        response["data"] = serde_json::json!("<redacted>");
        response
            .as_object_mut()
            .expect("object")
            .remove("truncated");
        assert_eq!(response[UntrustedObservation::FIELD], true);

        // Attaching again is idempotent, never a second value.
        assert!(label.attach(&mut response));
        assert_eq!(response[UntrustedObservation::FIELD], true);

        // A non-object cannot silently claim the label.
        let mut scalar = serde_json::json!("content");
        assert!(!label.attach(&mut scalar));
        assert_eq!(scalar, serde_json::json!("content"));
    }
}
