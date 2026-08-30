//! Suspicious-paste inspection and confirmation gate (P0-AC-008).
//!
//! Every paste entering the input pipeline is inspected for adversarial classes
//! and requires explicit confirmation when any class is present. There is no
//! silent delivery path: `request_paste` stores a pending paste when inspection
//! flags are set and only `confirm_pending_paste(true)` delivers it to the PTY
//! (bounded, headless-deterministic). Bracketed paste (`?2004`) is
//! defense-in-depth only — it wraps confirmed delivery but never bypasses
//! confirmation.
//!
//! Adversarial classes (each triggers `needs_confirmation`):
//! - C0 controls `0x00..0x1F` excluding safe subset (`\t` `0x09` is allowed; all
//!   other C0 including `0x00..0x08`, `0x0B`, `0x0C`, `0x0E..0x1F`)
//! - NUL `\0` (`0x00`)
//! - ESC `\x1b` (`0x1B`)
//! - CR `\r` (`0x0D`)
//! - Embedded newline `\n` (`0x0A`) — any presence is suspicious (single-line
//!   context is not known at the paste seam)
//! - Unicode controls `U+0080..U+009F` (C1 controls)
//! - BiDi / directional controls: `U+061C`, `U+200E`, `U+200F`, `U+202A..202E`,
//!   `U+2066..2069`, plus zero-width `U+200B..200D`, `U+FEFF`, `U+2060`

#![forbid(unsafe_code)]

/// Which suspicious classes were found in a paste.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PasteInspection {
    /// `true` when any `0x00..0x1F` C0 control (excluding `\t` allow-list)
    /// is present. This overlaps with the more specific NUL/ESC/CR/newline
    /// flags but is reported separately so every C0 byte is covered.
    pub has_c0: bool,
    /// `true` when NUL `0x00` is present.
    pub has_nul: bool,
    /// `true` when ESC `0x1B` is present.
    pub has_esc: bool,
    /// `true` when CR `0x0D` is present.
    pub has_cr: bool,
    /// `true` when LF `0x0A` (embedded newline) is present.
    pub has_newline: bool,
    /// `true` when Unicode C1 `U+0080..U+009F` is present.
    pub has_unicode_control: bool,
    /// `true` when any BiDi / directional control is present.
    pub has_bidi: bool,
}

impl PasteInspection {
    /// Whether any suspicious class was found and confirmation is required.
    #[must_use]
    pub fn needs_confirmation(&self) -> bool {
        self.has_c0
            || self.has_nul
            || self.has_esc
            || self.has_cr
            || self.has_newline
            || self.has_unicode_control
            || self.has_bidi
    }

    /// Human-readable reasons for inspection (bounded, deterministic order).
    #[must_use]
    pub fn reasons(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.has_nul {
            out.push("NUL");
        }
        if self.has_esc {
            out.push("ESC");
        }
        if self.has_cr {
            out.push("CR");
        }
        if self.has_newline {
            out.push("newline");
        }
        if self.has_c0 {
            out.push("C0");
        }
        if self.has_unicode_control {
            out.push("unicode-control");
        }
        if self.has_bidi {
            out.push("bidi");
        }
        out
    }

    /// Whether no flag is set.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.needs_confirmation()
    }
}

/// Inspect `text` for every adversarial class, bounded to the input length.
///
/// Deterministic and allocates only the returned struct (no heap per call
/// beyond the input). The input is already bounded by
/// `bitty_platform::clipboard::CLIPBOARD_MAX_BYTES` (8192) before this call
/// in the runtime paste path, so this scan is bounded `O(n)` with `n ≤ 8192`.
#[must_use]
pub fn inspect_paste(text: &str) -> PasteInspection {
    let mut insp = PasteInspection::default();
    for ch in text.chars() {
        let cp = ch as u32;
        // C0 and specific controls
        if cp <= 0x1F {
            // `\t` (0x09) is the only C0 allowed without C0 flag; every other
            // C0 is suspicious. NUL/ESC/CR/LF still set their specific flags too.
            if ch != '\t' {
                insp.has_c0 = true;
            }
            if ch == '\0' {
                insp.has_nul = true;
            }
            if ch == '\x1b' {
                insp.has_esc = true;
            }
            if ch == '\r' {
                insp.has_cr = true;
            }
            if ch == '\n' {
                insp.has_newline = true;
            }
        }
        // C1 controls U+0080..U+009F
        if (0x80..=0x9F).contains(&cp) {
            insp.has_unicode_control = true;
        }
        // BiDi and zero-width controls
        if is_bidi_control(ch) {
            insp.has_bidi = true;
        }
        // Early exit when all flags set — still deterministic.
        if insp.has_c0
            && insp.has_nul
            && insp.has_esc
            && insp.has_cr
            && insp.has_newline
            && insp.has_unicode_control
            && insp.has_bidi
        {
            break;
        }
    }
    insp
}

fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' // ARABIC LETTER MARK
        | '\u{200B}' // ZERO WIDTH SPACE
        | '\u{200C}' // ZERO WIDTH NON-JOINER
        | '\u{200D}' // ZERO WIDTH JOINER
        | '\u{200E}' // LEFT-TO-RIGHT MARK
        | '\u{200F}' // RIGHT-TO-LEFT MARK
        | '\u{202A}' // LRE
        | '\u{202B}' // RLE
        | '\u{202C}' // PDF
        | '\u{202D}' // LRO
        | '\u{202E}' // RLO
        | '\u{2060}' // WORD JOINER
        | '\u{2066}' // LRI
        | '\u{2067}' // RLI
        | '\u{2068}' // FSI
        | '\u{2069}' // PDI
        | '\u{FEFF}' // ZERO WIDTH NO-BREAK SPACE / BOM
    )
}

/// Pending paste that requires explicit confirmation before delivery.
///
/// Stored when `inspect_paste` finds suspicious content. The text is already
/// truncated to `CLIPBOARD_MAX_BYTES` at a char boundary before this struct is
/// created, so it is bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPaste {
    /// Bounded paste bytes as confirmed text.
    pub text: String,
    /// Inspection result for the pending text.
    pub inspection: PasteInspection,
}

impl PendingPaste {
    /// Create a pending entry from already-inspected text.
    #[must_use]
    pub fn new(text: String, inspection: PasteInspection) -> Self {
        Self { text, inspection }
    }
}

/// Wrap `text` with bracketed-paste delimiters when `bracketed` is true.
///
/// Defense-in-depth only: wrapping never bypasses the confirmation gate.
/// Caller must have already confirmed the paste.
#[must_use]
pub fn bracketed_wrap(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_needs_no_confirmation() {
        let insp = inspect_paste("hello world 123");
        assert!(insp.is_clean());
        assert!(!insp.needs_confirmation());
        assert!(insp.reasons().is_empty());
    }

    #[test]
    fn tab_is_allowed_without_c0_flag() {
        let insp = inspect_paste("a\tb");
        assert!(!insp.has_c0);
        assert!(insp.is_clean());
    }

    #[test]
    fn nul_triggers_c0_and_nul() {
        let insp = inspect_paste("a\0b");
        assert!(insp.has_nul);
        assert!(insp.has_c0);
        assert!(insp.needs_confirmation());
        assert!(insp.reasons().contains(&"NUL"));
    }

    #[test]
    fn esc_triggers_c0_and_esc() {
        let insp = inspect_paste("a\x1bb");
        assert!(insp.has_esc);
        assert!(insp.has_c0);
        assert!(insp.needs_confirmation());
    }

    #[test]
    fn cr_triggers_c0_and_cr() {
        let insp = inspect_paste("a\rb");
        assert!(insp.has_cr);
        assert!(insp.has_c0);
        assert!(insp.needs_confirmation());
    }

    #[test]
    fn newline_triggers_c0_and_newline() {
        let insp = inspect_paste("a\nb");
        assert!(insp.has_newline);
        assert!(insp.has_c0);
        assert!(insp.needs_confirmation());
    }

    #[test]
    fn c0_other_triggers_c0_only() {
        // BEL 0x07 and 0x01 are C0 but not NUL/ESC/CR/LF
        let insp = inspect_paste("a\x07b");
        assert!(insp.has_c0);
        assert!(!insp.has_nul);
        assert!(!insp.has_esc);
        assert!(!insp.has_cr);
        assert!(!insp.has_newline);
        assert!(insp.needs_confirmation());
        let insp2 = inspect_paste("a\x01b");
        assert!(insp2.has_c0);
    }

    #[test]
    fn unicode_control_u0080_to_009f() {
        let insp = inspect_paste("a\u{0080}b");
        assert!(insp.has_unicode_control);
        assert!(insp.needs_confirmation());
        let insp2 = inspect_paste("a\u{009F}b");
        assert!(insp2.has_unicode_control);
        let insp3 = inspect_paste("a\u{0090}b");
        assert!(insp3.has_unicode_control);
        // Not flagged for normal text
        let clean = inspect_paste("a\u{00A0}b"); // NBSP U+00A0 beyond 009F
        assert!(!clean.has_unicode_control);
    }

    #[test]
    fn bidi_controls_each_flag() {
        for ch in [
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
            '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{200B}', '\u{FEFF}',
            '\u{2060}',
        ] {
            let s = format!("a{ch}b");
            let insp = inspect_paste(&s);
            assert!(insp.has_bidi, "missing bidi for U+{:04X}", ch as u32);
            assert!(insp.needs_confirmation());
        }
    }

    #[test]
    fn multiple_classes_combined() {
        let insp = inspect_paste("a\0\n\u{0080}\u{202E}b");
        assert!(insp.has_nul);
        assert!(insp.has_newline);
        assert!(insp.has_unicode_control);
        assert!(insp.has_bidi);
        assert!(insp.has_c0);
        assert_eq!(
            insp.reasons(),
            vec!["NUL", "newline", "C0", "unicode-control", "bidi"]
        );
    }

    #[test]
    fn bracketed_wrap_defense_in_depth() {
        assert_eq!(bracketed_wrap("hi", false), b"hi".to_vec());
        assert_eq!(bracketed_wrap("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
        assert_eq!(bracketed_wrap("", true), b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    fn inspection_is_deterministic() {
        let a = inspect_paste("a\0\x1b\r\n\u{0080}\u{202E}");
        let b = inspect_paste("a\0\x1b\r\n\u{0080}\u{202E}");
        assert_eq!(a, b);
        assert_eq!(a.reasons(), b.reasons());
    }

    #[test]
    fn empty_is_clean() {
        assert!(inspect_paste("").is_clean());
    }
}
