//! Paste inspection and confirmation gate (P0-AC-008).
//!
//! Every paste entering the input pipeline is inspected, and text that is
//! unsafe to deliver silently requires explicit confirmation. There is no
//! silent delivery path: `request_paste` stores a pending paste when
//! inspection triggers and delivery happens only via explicit confirmation —
//! `confirm_pending_paste(true)`, repeating the identical paste while pending
//! (CTX-0186: second chord/right-click press with unchanged clipboard), or the
//! equivalent bracketed delivery after such confirmation. `Esc` while pending
//! cancels without delivery and without leaking `Esc` to the PTY. The pending
//! paste is always visible via `Runtime::pending_paste_summary` (bounded line
//! count, byte size, reasons, preview), so a gated paste is never silent.
//! Bracketed paste (`?2004`) is defense-in-depth only — it wraps confirmed
//! delivery but never bypasses confirmation.
//!
//! Two trigger groups require confirmation (`needs_confirmation`):
//!
//! 1. **Multi-line paste** — LF `\n` (`has_newline`). This is the kitty/ghostty
//!    safety default, not an attack classification: a paste that spans several
//!    lines can execute shell input in programs that do not handle bracketed
//!    paste. LF is the expected multi-line trigger, so it is reported by its own
//!    `newline` reason and is deliberately not folded into the generic `C0`
//!    class (CTX-0369).
//! 2. **Adversarial control classes** — each is reported by its own name:
//!    - NUL `\0` (`0x00`) — `has_nul`
//!    - ESC `\x1b` (`0x1B`) — `has_esc`
//!    - CR `\r` (`0x0D`) — `has_cr` (a carriage return submits the line)
//!    - Any other C0 control `0x00..0x1F` excluding tab and the
//!      specifically-named NUL/ESC/CR/LF — `has_c0`
//!    - Unicode controls `U+0080..U+009F` (C1 controls) — `has_unicode_control`
//!    - BiDi / directional controls: `U+061C`, `U+200E`, `U+200F`,
//!      `U+202A..202E`, `U+2066..2069`, plus zero-width `U+200B..200D`,
//!      `U+FEFF`, `U+2060` — `has_bidi`

#![forbid(unsafe_code)]

/// Which confirmation triggers were found in a paste: the multi-line LF
/// trigger and/or the adversarial control classes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PasteInspection {
    /// `true` when a C0 control `0x00..0x1F` other than the allow-listed
    /// `\t` (`0x09`) and the specifically-classified NUL/ESC/CR/LF is present.
    ///
    /// Every C0 byte still triggers confirmation, but through exactly one
    /// flag: NUL/ESC/CR/LF report their own names, and this flag covers only
    /// the remaining C0 controls (for example BEL `0x07`, BS `0x08`, FS `0x1C`).
    /// It therefore never duplicates a specific reason in [`Self::reasons`].
    pub has_c0: bool,
    /// `true` when NUL `0x00` is present.
    pub has_nul: bool,
    /// `true` when ESC `0x1B` is present.
    pub has_esc: bool,
    /// `true` when CR `0x0D` is present (a carriage return submits the line).
    pub has_cr: bool,
    /// `true` when LF `0x0A` is present. LF is the expected multi-line paste
    /// trigger (kitty/ghostty parity), reported as `newline`, not as `C0`.
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
    ///
    /// Production-visible (CTX-0186): the pending-paste summary surfaces these
    /// so a gated paste is never silent. At most 7 entries, `&'static str` only.
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
    #[cfg(test)]
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
pub(crate) fn inspect_paste(text: &str) -> PasteInspection {
    let mut insp = PasteInspection::default();
    for ch in text.chars() {
        let cp = ch as u32;
        // C0: exactly one flag per byte. `\t` (0x09) is the only C0 allowed
        // without a flag; NUL/ESC/CR/LF carry their own names so the generic
        // `C0` reason never duplicates them (CTX-0369); every other C0 is
        // reported as `C0`.
        if cp <= 0x1F {
            match ch {
                '\t' => {}
                '\0' => insp.has_nul = true,
                '\x1b' => insp.has_esc = true,
                '\r' => insp.has_cr = true,
                '\n' => insp.has_newline = true,
                _ => insp.has_c0 = true,
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
/// Stored when `inspect_paste` reports a confirmation trigger. The text is already
/// truncated to `CLIPBOARD_MAX_BYTES` at a char boundary before this struct is
/// created, so it is bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingPaste {
    /// Bounded paste bytes as confirmed text.
    pub(crate) text: String,
    /// Inspection result for the pending text.
    pub(crate) inspection: PasteInspection,
}

impl PendingPaste {
    /// Create a pending entry from already-inspected text.
    #[must_use]
    pub(crate) fn new(text: String, inspection: PasteInspection) -> Self {
        Self { text, inspection }
    }
}

/// Wrap `text` with bracketed-paste delimiters when `bracketed` is true.
///
/// Defense-in-depth only: wrapping never bypasses the confirmation gate.
/// Caller must have already confirmed the paste.
#[must_use]
pub(crate) fn bracketed_wrap(text: &str, bracketed: bool) -> Vec<u8> {
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
    fn nul_triggers_nul_only() {
        let insp = inspect_paste("a\0b");
        assert!(insp.has_nul);
        assert!(!insp.has_c0, "NUL must not also be reported as generic C0");
        assert!(insp.needs_confirmation());
        assert_eq!(insp.reasons(), vec!["NUL"]);
    }

    #[test]
    fn esc_triggers_esc_only() {
        let insp = inspect_paste("a\x1bb");
        assert!(insp.has_esc);
        assert!(!insp.has_c0, "ESC must not also be reported as generic C0");
        assert!(insp.needs_confirmation());
        assert_eq!(insp.reasons(), vec!["ESC"]);
    }

    #[test]
    fn cr_triggers_cr_only() {
        let insp = inspect_paste("a\rb");
        assert!(insp.has_cr);
        assert!(!insp.has_c0, "CR must not also be reported as generic C0");
        assert!(insp.needs_confirmation());
        assert_eq!(insp.reasons(), vec!["CR"]);
    }

    #[test]
    fn newline_triggers_multiline_only() {
        // CTX-0369: LF is the expected multi-line trigger, not a control-char
        // attack. It still gates, but is reported as `newline`, not `C0`.
        let insp = inspect_paste("line1\nline2");
        assert!(insp.has_newline);
        assert!(!insp.has_c0, "LF must not be reported as generic C0");
        assert!(insp.needs_confirmation());
        assert_eq!(insp.reasons(), vec!["newline"]);
    }

    #[test]
    fn crlf_reports_cr_and_newline_not_c0() {
        let insp = inspect_paste("a\r\nb");
        assert!(insp.has_cr);
        assert!(insp.has_newline);
        assert!(!insp.has_c0);
        assert!(insp.needs_confirmation());
        assert_eq!(insp.reasons(), vec!["CR", "newline"]);
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
        assert!(
            !insp.has_c0,
            "only NUL/LF present; neither may emit generic C0"
        );
        assert_eq!(
            insp.reasons(),
            vec!["NUL", "newline", "unicode-control", "bidi"]
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
