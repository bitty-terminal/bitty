//! Kitty font-query intake (CTX-1370, issue #2263).
//!
//! Detectors such as fastfetch ask a terminal for its resolved font through
//! a DCS query shaped like:
//!
//! ```text
//! ESC P +q <hex-key>[;<hex-key>...] ST
//! ```
//!
//! The same payload is also accepted in the APC spelling (`ESC _ +q ... ST`)
//! because the requirement named APC; real clients (fastfetch, kitty) send
//! the DCS form, which shares the XTGETTCAP envelope.
//!
//! Each wire token hex-decodes to a query key. fastfetch sends both keys in
//! one query and expects one reply per key (see
//! [`crate::TerminalAction::KittyFontQuery`] and the runtime reply builder),
//! in request order:
//!
//! ```text
//! ESC P 1+r kitty-query-font_family=<hex-value> ST
//! ESC P 1+r kitty-query-font_size=<hex-value> ST
//! ```
//!
//! The parser only classifies and bounds the request. Font values are
//! configuration-owned (the runtime resolves them), so terminal state stays
//! inert and this crate never fabricates a reply. Unknown keys are ignored
//! silently; malformed hex, empty payloads, and payloads above
//! [`KITTY_FONT_QUERY_WIRE_MAX_BYTES`] produce no action at all.
//!
//! # Bounds
//!
//! Decoding uses one fixed stack buffer ([`KITTY_FONT_QUERY_DECODED_MAX_BYTES`]);
//! no heap allocation scales with the query. At most
//! [`KITTY_FONT_QUERY_MAX_KEYS`] wire tokens are examined per APC, so a
//! hostile stream cannot amplify one sequence into unbounded work.
//!
//! # Wire forms
//!
//! Both forms seen in the wild decode identically:
//!
//! - token-per-key with literal separators (`hex(key1);hex(key2)`, what
//!   fastfetch sends), and
//! - one hex token whose decoded text contains the separator
//!   (`hex("key1;key2")`).

#![forbid(unsafe_code)]

/// APC payload prefix that marks a kitty font query (`ESC P + q ... ST`).
pub const KITTY_FONT_QUERY_PREFIX: &[u8] = b"+q";

/// Maximum raw payload bytes accepted after [`KITTY_FONT_QUERY_PREFIX`].
///
/// The two canonical keys plus separators need 54 wire bytes; 512 leaves
/// room for future keys while keeping per-sequence work trivially bounded.
pub const KITTY_FONT_QUERY_WIRE_MAX_BYTES: usize = 512;

/// Maximum decoded payload bytes held in the fixed stack buffer.
///
/// 128 bytes cover every known key several times over; larger declarations
/// are refused before any classification work.
pub const KITTY_FONT_QUERY_DECODED_MAX_BYTES: usize = 128;

/// Maximum wire tokens examined per query payload.
pub const KITTY_FONT_QUERY_MAX_KEYS: usize = 8;

/// Query key for the resolved font family.
pub const KITTY_FONT_QUERY_FAMILY: &[u8] = b"kitty-query-font_family";

/// Query key for the resolved font size.
pub const KITTY_FONT_QUERY_SIZE: &[u8] = b"kitty-query-font_size";

/// The known keys requested by one kitty font-query APC.
///
/// Only the two keys bitty can answer truthfully are represented; unknown
/// keys never reach this type (they are ignored at classification time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KittyFontQuery {
    /// `kitty-query-font_family` was requested.
    pub family: bool,
    /// `kitty-query-font_size` was requested.
    pub size: bool,
}

impl KittyFontQuery {
    /// Whether at least one answerable key was requested.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.family && !self.size
    }
}

/// Classifies one decoded key token into the known-key set.
fn note_key(token: &[u8], out: &mut KittyFontQuery) {
    if token == KITTY_FONT_QUERY_FAMILY {
        out.family = true;
    } else if token == KITTY_FONT_QUERY_SIZE {
        out.size = true;
    }
}

/// Decodes an even-length hexadecimal token into `out`.
///
/// Returns the decoded length, or `None` for an odd length, a non-hex byte,
/// or a token that would exceed `out`.
fn decode_hex_into(token: &[u8], out: &mut [u8]) -> Option<usize> {
    if token.len() % 2 != 0 || token.len() / 2 > out.len() {
        return None;
    }
    let mut len = 0usize;
    for pair in token.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out[len] = (hi << 4) | lo;
        len += 1;
    }
    Some(len)
}

/// One ASCII hex nibble (`0-9`, `a-f`, `A-F`).
const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Classifies every `;`-separated key in one decoded payload buffer.
fn note_decoded_keys(decoded: &[u8], out: &mut KittyFontQuery) {
    for token in decoded.split(|byte| *byte == b';') {
        note_key(token, out);
    }
}

/// Parses a raw APC payload beginning with [`KITTY_FONT_QUERY_PREFIX`].
///
/// Returns `None` when the payload is not a font query, is empty, exceeds
/// the wire or decoded bound, contains malformed hex, or names no known key.
/// Unknown keys are ignored, so a mixed query still answers its known keys.
#[must_use]
pub fn parse_kitty_font_query(payload: &[u8]) -> Option<KittyFontQuery> {
    parse_kitty_font_query_body(payload.strip_prefix(KITTY_FONT_QUERY_PREFIX)?)
}

/// Parses the payload body after the `+q` introducer.
///
/// This is the same classifier used for DCS `ESC P +q <body> ST` (the wire
/// shape fastfetch and kitty actually send) and for the APC spelling.
#[must_use]
pub fn parse_kitty_font_query_body(body: &[u8]) -> Option<KittyFontQuery> {
    if body.is_empty() || body.len() > KITTY_FONT_QUERY_WIRE_MAX_BYTES {
        return None;
    }

    let mut decoded = [0u8; KITTY_FONT_QUERY_DECODED_MAX_BYTES];
    let mut query = KittyFontQuery::default();

    if body.contains(&b';') {
        // fastfetch shape: one hex token per key, literal `;` separators.
        for token in body
            .split(|byte| *byte == b';')
            .take(KITTY_FONT_QUERY_MAX_KEYS)
        {
            let len = decode_hex_into(token, &mut decoded)?;
            note_key(&decoded[..len], &mut query);
        }
    } else {
        // Spec shape: one hex token whose decoded text carries the separators.
        let len = decode_hex_into(body, &mut decoded)?;
        note_decoded_keys(&decoded[..len], &mut query);
    }

    (!query.is_empty()).then_some(query)
}

/// Hex spelling of the reserved `kitty-query-` key prefix.
const KITTY_QUERY_FONT_HEX_PREFIX: &[u8] = b"6b697474792d71756572792d";

/// Whether a DCS `+q` payload belongs to the kitty font-query family.
///
/// The XTGETTCAP and kitty font protocols share the `ESC P +q ... ST`
/// envelope. Reserved `kitty-query-*` keys must never be answered as
/// capabilities (a negative `DCS 0 + r` would be a second, wrong reply), and
/// malformed font queries must stay silent. The hex prefix is checked
/// case-insensitively so uppercase encoders are still classified as font
/// queries even when their remaining hex is malformed.
#[must_use]
pub fn looks_like_kitty_font_query(body: &[u8]) -> bool {
    body.len() >= KITTY_QUERY_FONT_HEX_PREFIX.len()
        && body[..KITTY_QUERY_FONT_HEX_PREFIX.len()]
            .eq_ignore_ascii_case(KITTY_QUERY_FONT_HEX_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> Vec<u8> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = Vec::with_capacity(bytes.len() * 2);
        for &byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize]);
            out.push(DIGITS[(byte & 0x0f) as usize]);
        }
        out
    }

    fn query(keys: &[&[u8]]) -> Vec<u8> {
        let mut wire = b"+q".to_vec();
        for (index, key) in keys.iter().enumerate() {
            if index > 0 {
                wire.push(b';');
            }
            wire.extend_from_slice(&hex(key));
        }
        wire
    }

    #[test]
    fn fastfetch_shape_yields_both_keys() {
        let wire = query(&[KITTY_FONT_QUERY_FAMILY, KITTY_FONT_QUERY_SIZE]);
        assert_eq!(
            parse_kitty_font_query(&wire),
            Some(KittyFontQuery {
                family: true,
                size: true
            })
        );
    }

    #[test]
    fn single_key_shapes_are_classified() {
        assert_eq!(
            parse_kitty_font_query(&query(&[KITTY_FONT_QUERY_SIZE])),
            Some(KittyFontQuery {
                family: false,
                size: true
            })
        );
        assert_eq!(
            parse_kitty_font_query(&query(&[KITTY_FONT_QUERY_FAMILY])),
            Some(KittyFontQuery {
                family: true,
                size: false
            })
        );
    }

    #[test]
    fn decoded_separator_form_is_accepted() {
        let wire = [
            b"+q".as_slice(),
            &hex(b"kitty-query-font_family;kitty-query-font_size"),
        ]
        .concat();
        assert_eq!(
            parse_kitty_font_query(&wire),
            Some(KittyFontQuery {
                family: true,
                size: true
            })
        );
    }

    #[test]
    fn unknown_keys_are_ignored_without_error() {
        assert_eq!(parse_kitty_font_query(&query(&[b"kitty-query-nope"])), None);
        assert_eq!(
            parse_kitty_font_query(&query(&[b"kitty-query-nope", KITTY_FONT_QUERY_SIZE])),
            Some(KittyFontQuery {
                family: false,
                size: true
            })
        );
    }

    #[test]
    fn malformed_hex_is_refused() {
        assert_eq!(parse_kitty_font_query(b"+q6b697474792d7175657279"), None);
        assert_eq!(parse_kitty_font_query(b"+q6b69747479zz"), None);
        assert_eq!(parse_kitty_font_query(b"+q6"), None);
    }

    #[test]
    fn empty_and_non_query_payloads_are_inert() {
        assert_eq!(parse_kitty_font_query(b""), None);
        assert_eq!(parse_kitty_font_query(b"+q"), None);
        assert_eq!(parse_kitty_font_query(b"Gf=24;AAAA"), None);
        assert_eq!(parse_kitty_font_query(b"hello"), None);
    }

    #[test]
    fn oversized_wire_is_refused() {
        let mut wire = b"+q".to_vec();
        wire.extend(std::iter::repeat_n(
            b'a',
            KITTY_FONT_QUERY_WIRE_MAX_BYTES + 1,
        ));
        assert_eq!(parse_kitty_font_query(&wire), None);
    }

    #[test]
    fn token_count_is_bounded() {
        // More tokens than the cap: only the bounded prefix is examined.
        let keys: Vec<&[u8]> = std::iter::repeat_n(b"kitty-query-nope" as &[u8], 64).collect();
        assert_eq!(parse_kitty_font_query(&query(&keys)), None);
    }

    #[test]
    fn uppercase_hex_is_accepted() {
        let wire = b"+q6B697474792D71756572792D666F6E745F73697A65";
        assert_eq!(
            parse_kitty_font_query(wire),
            Some(KittyFontQuery {
                family: false,
                size: true
            })
        );
    }

    #[test]
    fn body_form_matches_prefixed_form() {
        let body = hex(b"kitty-query-font_family;kitty-query-font_size");
        assert_eq!(
            parse_kitty_font_query_body(&body),
            parse_kitty_font_query(&[b"+q".as_slice(), body.as_slice()].concat())
        );
    }

    #[test]
    fn lookalike_covers_malformed_and_unknown_font_queries() {
        assert!(looks_like_kitty_font_query(&hex(
            b"kitty-query-font_family"
        )));
        assert!(looks_like_kitty_font_query(
            b"6B697474792D71756572792D6E6F7065"
        ));
        assert!(looks_like_kitty_font_query(b"6b697474792d71756572792dzz"));
        assert!(!looks_like_kitty_font_query(b"544E"));
        assert!(!looks_like_kitty_font_query(b""));
        assert!(!looks_like_kitty_font_query(b"6b69747479"));
    }
}
