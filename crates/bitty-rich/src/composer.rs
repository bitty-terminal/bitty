//! Command Composer: multiline buffer + bracketed-paste submit + external editor
//! (CTX-0227, 008 route steps 4-5 / P4+P5).
//!
//! Design inputs (read-only): `recording/research/008.md.completed`
//! sections 11-18. P1 anchoring + P2 fold landed in #390, P3 Hint Mode in
//! #392; this slice is P4 (composer + submit) + P5 (external editor) only.
//!
//! # What this module is
//!
//! A headless, bounded multiline command buffer that is **explicitly opened**
//! and never hijacks Normal Mode input:
//!
//! ```text
//! Raw Terminal Input
//!         │
//!         ├── Normal Mode (closed) → PTY byte-identical
//!         └── Composer Mode (open) → ComposerSession::feed
//! ```
//!
//! * **Composer Mode** (008 section 12): [`CommandBuffer`] holds the
//!   multiline text. [`ComposerKeys`] defaults to `Enter = newline`,
//!   `Ctrl+Enter = execute`, `Esc = close`, `Alt+E = external editor`
//!   (008 section 13); every binding is replaceable via
//!   [`ComposerKeys::from_chords`] so config can remap them (e.g.
//!   `submit = Enter`, `newline = Shift+Enter`).
//! * **Single-owner open** (same rule as Hint arming in [`crate::hints`]):
//!   the composer opens only on a modifier chord registered in the existing
//!   keymap (suggested [`COMPOSER_OPEN_CHORD`] = `"alt+e"`, the `Leader+e`
//!   slot). [`validate_open_chord`] fails closed on bare single-char chords
//!   so the open key can never shadow shell typing; the keymap's
//!   `match_keymap` remains the single owner (a bound chord is consumed by
//!   chrome, an unbound key always reaches the PTY).
//! * **Submit path** (008 section 18): [`frame_submit`] turns the buffer
//!   into one bracketed-paste frame
//!   `ESC[200~ + content + ESC[201~ + CR`. No keystroke simulation: the
//!   caller writes the returned bytes to the PTY in a single write and the
//!   shell line editor receives Unicode/multiline content paste-safely.
//! * **External editor** (008 section 16): [`edit_externally`] writes the
//!   buffer to a `0600` temp file under the OS temp dir, spawns
//!   `$VISUAL`/`$EDITOR` ([`resolve_editor`]) with a bounded timeout plus
//!   kill, reads the file back, deletes it, and returns to the composer.
//!   Every failure is fail-closed: the buffer is untouched and the temp
//!   file is still deleted ([`TempComposerFile`] RAII guard).
//!
//! # Hard boundary (008 section 14, load-bearing)
//!
//! The composer **never** globally hijacks `Enter`. While
//! [`ComposerSession`] is closed, [`normal_mode_passthrough`] returns input
//! bytes unchanged (vim/ssh/repl/TUI unaffected — pinned by
//! `normal_mode_enter_is_byte_identical`). The composer engages only after
//! an explicit [`ComposerSession::open`]. OSC-133 prompt-aware auto-offer
//! (008 section 15) is a follow-up, NOT this slice:
//! [`should_auto_offer`] always returns `false` (fail-open default: manual
//! open only), so no caller can accidentally auto-enter the composer.
//!
//! # Terminal Truth
//!
//! Nothing here touches [`State`](bitty_term_state::State): no grid write,
//! no scrollback push/clear/resize, no zone/cwd edit. The buffer lives in
//! [`CommandBuffer`]; submit bytes are returned for the caller to write;
//! editor round-trips go through an OS temp file, never through terminal
//! state.
//!
//! # Bounds (threat T-01)
//!
//! | Collection | Cap | Policy |
//! |---|---|---|
//! | [`CommandBuffer`] content | [`COMPOSER_MAX_BYTES`] (64 KiB) | insert fails closed, buffer kept |
//! | [`frame_submit`] output | `content + 13` bytes | `TooLarge` before framing, nothing emitted |
//! | temp file write/read | [`COMPOSER_MAX_BYTES`] | `TooLarge` fail-closed, file still deleted |
//! | editor wait | caller `timeout`, capped at [`EDITOR_TIMEOUT_MAX`] (300 s) | kill + `Timeout` error |
//! | editor path/args | one file arg only, no shell | spaces in path are data, never split |
//!
//! No I/O except the editor round-trip, no wall-clock except the editor
//! timeout poll loop, no randomness for content (temp names mix pid +
//! nanos + counter only), no unsafe.

#![forbid(unsafe_code)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Bounds and framing constants
// ---------------------------------------------------------------------------

/// Maximum composer buffer / temp-file payload in bytes (64 KiB).
///
/// Large enough for long `docker`/`kubectl`/`jq`/SQL one-liners and small
/// heredocs; bounded so the PTY write, the temp file, and the read-back are
/// all `O(1)`-capped. Inserts past the cap fail closed with
/// [`BufferError::TooLarge`] and the buffer is kept as-is.
pub const COMPOSER_MAX_BYTES: usize = 64 * 1024;

/// Bracketed-paste open marker (`ESC[200~`), byte-exact.
pub const PASTE_OPEN: &str = "\u{1b}[200~";
/// Bracketed-paste close marker (`ESC[201~`), byte-exact.
pub const PASTE_CLOSE: &str = "\u{1b}[201~";
/// Final submit terminator: `CR` (`Enter` VT encoding, same as
/// `bitty-platform` `Enter → "\r"`).
pub const SUBMIT_TERMINATOR: &str = "\r";

/// Suggested keymap chord that opens the composer (`Leader+e` slot).
///
/// A modifier chord on purpose: it can be registered through the existing
/// single-owner keymap (`{ chord = "alt+e", action = "open_composer" }`)
/// without ever shadowing bare `e` shell typing. Any modifier chord parses
/// via [`validate_open_chord`]; this constant is only the default suggestion.
pub const COMPOSER_OPEN_CHORD: &str = "alt+e";

/// Default editor wait before kill.
pub const EDITOR_TIMEOUT_DEFAULT: Duration = Duration::from_secs(120);
/// Maximum editor wait (fail-closed cap; larger requests are clamped).
pub const EDITOR_TIMEOUT_MAX: Duration = Duration::from_secs(300);
/// Poll interval while waiting for the editor child.
const EDITOR_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Temp file name prefix (inside [`std::env::temp_dir`]).
const TEMP_PREFIX: &str = "bitty-composer-";

// ---------------------------------------------------------------------------
// CommandBuffer
// ---------------------------------------------------------------------------

/// Why a buffer mutation failed (all fail-closed: buffer kept as-is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BufferError {
    /// Content would exceed [`COMPOSER_MAX_BYTES`].
    TooLarge {
        /// Bytes the content would have had.
        wanted: usize,
    },
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { wanted } => {
                write!(
                    f,
                    "composer buffer too large ({wanted} bytes, max {COMPOSER_MAX_BYTES})"
                )
            }
        }
    }
}

impl std::error::Error for BufferError {}

/// Multiline command buffer, bounded at [`COMPOSER_MAX_BYTES`] bytes.
///
/// Plain UTF-8 text; newlines are `\n`. No cursor model in this slice:
/// inserts append (the external editor covers random-access editing) and
/// the headless tests pin append/newline/unicode/cap semantics.
#[derive(Debug, Clone, Default)]
pub struct CommandBuffer {
    text: String,
}

impl CommandBuffer {
    /// Empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
        }
    }

    /// Buffer pre-filled with `content` (fails closed past the cap).
    ///
    /// # Errors
    /// [`BufferError::TooLarge`] when `content` exceeds the byte cap.
    pub fn with_content(content: &str) -> Result<Self, BufferError> {
        if content.len() > COMPOSER_MAX_BYTES {
            return Err(BufferError::TooLarge {
                wanted: content.len(),
            });
        }
        Ok(Self {
            text: content.to_string(),
        })
    }

    /// Appends `s` (fails closed past the cap, buffer kept).
    ///
    /// # Errors
    /// [`BufferError::TooLarge`] when the result would exceed the cap.
    pub fn insert_str(&mut self, s: &str) -> Result<(), BufferError> {
        let wanted = self.text.len().saturating_add(s.len());
        if wanted > COMPOSER_MAX_BYTES {
            return Err(BufferError::TooLarge { wanted });
        }
        self.text.push_str(s);
        Ok(())
    }

    /// Appends one char (fails closed past the cap, buffer kept).
    ///
    /// # Errors
    /// [`BufferError::TooLarge`] when the result would exceed the cap.
    pub fn push_char(&mut self, c: char) -> Result<(), BufferError> {
        let wanted = self.text.len().saturating_add(c.len_utf8());
        if wanted > COMPOSER_MAX_BYTES {
            return Err(BufferError::TooLarge { wanted });
        }
        self.text.push(c);
        Ok(())
    }

    /// Appends `\n` (fails closed past the cap, buffer kept).
    ///
    /// # Errors
    /// [`BufferError::TooLarge`] when the result would exceed the cap.
    pub fn push_newline(&mut self) -> Result<(), BufferError> {
        self.insert_str("\n")
    }

    /// Replaces the whole content (fails closed past the cap, kept as-is).
    ///
    /// # Errors
    /// [`BufferError::TooLarge`] when `content` exceeds the byte cap.
    pub fn set(&mut self, content: &str) -> Result<(), BufferError> {
        if content.len() > COMPOSER_MAX_BYTES {
            return Err(BufferError::TooLarge {
                wanted: content.len(),
            });
        }
        self.text.clear();
        self.text.push_str(content);
        Ok(())
    }

    /// Clears the buffer.
    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// Current content.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Content length in bytes.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.text.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Composer keys (customizable, defaults per 008 section 13)
// ---------------------------------------------------------------------------

/// Named key a composer binding can sit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComposerKey {
    /// `Enter` / `Return`.
    Enter,
    /// `Escape` / `Esc`.
    Escape,
    /// Single ASCII character, stored lowercase (`Char('e')`).
    Char(char),
}

impl ComposerKey {
    /// Canonical spelling (`enter`, `escape`, `e`).
    #[must_use]
    pub fn canonical(self) -> String {
        match self {
            Self::Enter => "enter".to_string(),
            Self::Escape => "escape".to_string(),
            Self::Char(c) => c.to_string(),
        }
    }
}

/// One composer binding: key plus held modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComposerChord {
    /// The key itself.
    pub key: ComposerKey,
    /// Control held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
}

impl ComposerChord {
    /// `Enter` with no modifiers (default newline).
    #[must_use]
    pub const fn enter() -> Self {
        Self {
            key: ComposerKey::Enter,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// `Ctrl+Enter` (default submit).
    #[must_use]
    pub const fn ctrl_enter() -> Self {
        Self {
            key: ComposerKey::Enter,
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    /// `Esc` with no modifiers (default close).
    #[must_use]
    pub const fn escape() -> Self {
        Self {
            key: ComposerKey::Escape,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// `Alt+E` (default external editor).
    #[must_use]
    pub const fn alt_e() -> Self {
        Self {
            key: ComposerKey::Char('e'),
            ctrl: false,
            alt: true,
            shift: false,
        }
    }

    /// Canonical spelling (`enter`, `ctrl+enter`, `escape`, `alt+e`).
    #[must_use]
    pub fn canonical(self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("ctrl+");
        }
        if self.alt {
            out.push_str("alt+");
        }
        if self.shift {
            out.push_str("shift+");
        }
        out.push_str(&self.key.canonical());
        out
    }

    /// Parses `Enter`, `Ctrl+Enter`, `Esc`/`Escape`, `Alt+E`,
    /// `Shift+Enter`, or `<mod>+<char>` (fail-closed).
    ///
    /// # Errors
    /// [`ChordParseError`] on empty/unknown input, bare multi-char keys,
    /// or modifier-only input.
    pub fn parse(raw: &str) -> Result<Self, ChordParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ChordParseError::Empty);
        }
        if trimmed.len() > 64 {
            return Err(ChordParseError::Unknown(trimmed.to_string()));
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut key: Option<ComposerKey> = None;
        for part in trimmed.split('+') {
            let token = part.trim().to_ascii_lowercase();
            if token.is_empty() {
                return Err(ChordParseError::Unknown(trimmed.to_string()));
            }
            match token.as_str() {
                "ctrl" | "control" => {
                    if ctrl {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    ctrl = true;
                }
                "alt" | "opt" | "option" => {
                    if alt {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    alt = true;
                }
                "shift" => {
                    if shift {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    shift = true;
                }
                "enter" | "return" => {
                    if key.is_some() {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    key = Some(ComposerKey::Enter);
                }
                "escape" | "esc" => {
                    if key.is_some() {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    key = Some(ComposerKey::Escape);
                }
                _ => {
                    if key.is_some() {
                        return Err(ChordParseError::Unknown(trimmed.to_string()));
                    }
                    let mut chars = token.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if c.is_ascii_graphic() => {
                            key = Some(ComposerKey::Char(c.to_ascii_lowercase()));
                        }
                        _ => return Err(ChordParseError::Unknown(trimmed.to_string())),
                    }
                }
            }
        }
        key.map(|key| Self {
            key,
            ctrl,
            alt,
            shift,
        })
        .ok_or_else(|| ChordParseError::Unknown(trimmed.to_string()))
    }

    /// Matches a headless key event (exact equality, single owner).
    #[must_use]
    pub fn matches_event(self, ev: ComposerKeyEvent) -> bool {
        let key_eq = match (self.key, ev.key) {
            (ComposerKey::Enter, ComposerKey::Enter) => true,
            (ComposerKey::Escape, ComposerKey::Escape) => true,
            (ComposerKey::Char(a), ComposerKey::Char(b)) => a.eq_ignore_ascii_case(&b),
            _ => false,
        };
        key_eq && self.ctrl == ev.ctrl && self.alt == ev.alt && self.shift == ev.shift
    }
}

/// Why a composer chord string failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordParseError {
    /// Empty input.
    Empty,
    /// Unknown token/shape (echoes the raw input for diagnostics).
    Unknown(String),
}

impl std::fmt::Display for ChordParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("composer chord must not be empty"),
            Self::Unknown(raw) => write!(
                f,
                "unknown composer chord '{raw}'; expected Enter, Ctrl+Enter, Shift+Enter, Escape, or <mod>+<char> (e.g. Alt+E)"
            ),
        }
    }
}

impl std::error::Error for ChordParseError {}

/// The four composer bindings (all customizable; defaults per 008 §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposerKeys {
    /// Inserts `\n` (default `Enter`).
    pub newline: ComposerChord,
    /// Frames + returns submit bytes (default `Ctrl+Enter`).
    pub submit: ComposerChord,
    /// Closes the composer, buffer preserved (default `Esc`).
    pub close: ComposerChord,
    /// Requests the external-editor round-trip (default `Alt+E`).
    pub external_editor: ComposerChord,
}

impl Default for ComposerKeys {
    fn default() -> Self {
        Self {
            newline: ComposerChord::enter(),
            submit: ComposerChord::ctrl_enter(),
            close: ComposerChord::escape(),
            external_editor: ComposerChord::alt_e(),
        }
    }
}

/// Why custom composer keys were rejected (fail-closed: keep old keys).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerKeysError {
    /// Chord string failed to parse.
    Parse(ChordParseError),
    /// Two roles share one chord (echoes the canonical duplicates).
    Duplicate(Vec<String>),
}

impl std::fmt::Display for ComposerKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "composer keys: {e}"),
            Self::Duplicate(dups) => {
                write!(f, "composer keys must be distinct; duplicates: ")?;
                for (i, d) in dups.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "'{d}'")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ComposerKeysError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Duplicate(_) => None,
        }
    }
}

impl From<ChordParseError> for ComposerKeysError {
    fn from(value: ChordParseError) -> Self {
        Self::Parse(value)
    }
}

impl ComposerKeys {
    /// Builds custom keys from chord strings, e.g.
    /// `newline = "Shift+Enter"`, `submit = "Enter"`.
    ///
    /// The four roles must be pairwise distinct so one press can never
    /// mean two things.
    ///
    /// # Errors
    /// [`ComposerKeysError::Parse`] on bad chord strings;
    /// [`ComposerKeysError::Duplicate`] when roles collide.
    pub fn from_chords(
        newline: &str,
        submit: &str,
        close: &str,
        external_editor: &str,
    ) -> Result<Self, ComposerKeysError> {
        let keys = Self {
            newline: ComposerChord::parse(newline)?,
            submit: ComposerChord::parse(submit)?,
            close: ComposerChord::parse(close)?,
            external_editor: ComposerChord::parse(external_editor)?,
        };
        keys.validate()?;
        Ok(keys)
    }

    /// Rejects colliding roles (fail-closed).
    ///
    /// # Errors
    /// [`ComposerKeysError::Duplicate`] when any two roles are equal.
    pub fn validate(self) -> Result<Self, ComposerKeysError> {
        let chords = [
            self.newline.canonical(),
            self.submit.canonical(),
            self.close.canonical(),
            self.external_editor.canonical(),
        ];
        let mut dups: Vec<String> = Vec::new();
        for i in 0..chords.len() {
            for j in (i + 1)..chords.len() {
                if chords[i] == chords[j] && !dups.contains(&chords[i]) {
                    dups.push(chords[i].clone());
                }
            }
        }
        if dups.is_empty() {
            Ok(self)
        } else {
            Err(ComposerKeysError::Duplicate(dups))
        }
    }
}

// ---------------------------------------------------------------------------
// Open chord: single-owner gate (mirrors Hint arming)
// ---------------------------------------------------------------------------

/// Validated composer open chord (always carries a modifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpenChord {
    /// Lowercase bare key (`'e'` for `alt+e`).
    pub key: char,
    /// Control held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
}

/// Why the composer open chord was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenChordError {
    /// Empty input.
    Empty,
    /// Bare single-char chord with no modifier (would shadow shell typing).
    BareKey(char),
    /// Anything else (unknown key/modifier shape).
    Unknown(String),
}

impl std::fmt::Display for OpenChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("composer open chord must not be empty"),
            Self::BareKey(c) => write!(
                f,
                "composer open chord '{c}' needs a modifier (e.g. 'alt+{c}'); bare keys go to the shell"
            ),
            Self::Unknown(raw) => write!(
                f,
                "unknown composer open chord '{raw}'; expected '<mod>+<char>' (e.g. 'alt+e')"
            ),
        }
    }
}

impl std::error::Error for OpenChordError {}

/// Validates the chord a keymap would bind to `open_composer`.
///
/// Accepts exactly `<mod>+...+<single-char>` (any order, case-insensitive;
/// named keys like `f5` are rejected — the open slot is a Leader-style
/// letter chord). Bare single-char chords fail closed with
/// [`OpenChordError::BareKey`] so registration can never shadow shell
/// typing; this is the same single-owner rule Hint arming uses.
///
/// # Errors
/// [`OpenChordError`] on empty, bare, or unknown input.
pub fn validate_open_chord(raw: &str) -> Result<OpenChord, OpenChordError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OpenChordError::Empty);
    }
    if trimmed.len() > 64 {
        return Err(OpenChordError::Unknown(trimmed.to_string()));
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_held = false;
    let mut key: Option<char> = None;
    for part in trimmed.split('+') {
        let token = part.trim().to_ascii_lowercase();
        if token.is_empty() {
            return Err(OpenChordError::Unknown(trimmed.to_string()));
        }
        match token.as_str() {
            "ctrl" | "control" => {
                if ctrl {
                    return Err(OpenChordError::Unknown(trimmed.to_string()));
                }
                ctrl = true;
            }
            "alt" | "opt" | "option" => {
                if alt {
                    return Err(OpenChordError::Unknown(trimmed.to_string()));
                }
                alt = true;
            }
            "shift" => {
                if shift {
                    return Err(OpenChordError::Unknown(trimmed.to_string()));
                }
                shift = true;
            }
            "super" | "meta" | "cmd" | "command" | "win" | "windows" => {
                if super_held {
                    return Err(OpenChordError::Unknown(trimmed.to_string()));
                }
                super_held = true;
            }
            _ => {
                if key.is_some() {
                    return Err(OpenChordError::Unknown(trimmed.to_string()));
                }
                let mut chars = token.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if c.is_ascii_graphic() => {
                        key = Some(c.to_ascii_lowercase());
                    }
                    _ => return Err(OpenChordError::Unknown(trimmed.to_string())),
                }
            }
        }
    }
    let Some(key) = key else {
        return Err(OpenChordError::Unknown(trimmed.to_string()));
    };
    if !(ctrl || alt || shift || super_held) {
        return Err(OpenChordError::BareKey(key));
    }
    // `OpenChord` tracks ctrl/alt/shift only by design (the open slot is a
    // Leader-style alt/ctrl letter chord): a super-only chord still passes
    // the bare-key gate above (no shadowing risk) and is reported with the
    // real ctrl/alt/shift bits, so callers must treat a validated chord as
    // modifier-bearing even when all three bits are false.
    Ok(OpenChord {
        key,
        ctrl,
        alt,
        shift,
    })
}

// ---------------------------------------------------------------------------
// Session + feed (Normal vs Composer routing)
// ---------------------------------------------------------------------------

/// Headless key event fed to [`ComposerSession::feed`].
///
/// Mirrors the fields the app builds from its `KeyEvent` plus tracked
/// modifiers; `text` carries printable input while the composer is open
/// (single chars and pasted snippets alike append to the buffer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerKeyEvent {
    /// Pressed key.
    pub key: ComposerKey,
    /// Control held.
    pub ctrl: bool,
    /// Alt held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
    /// Printable text for this press, if any.
    pub text: Option<String>,
}

impl ComposerKeyEvent {
    /// Builds a printable-char press (`text` = the char).
    #[must_use]
    pub fn printable(c: char) -> Self {
        Self {
            key: ComposerKey::Char(c.to_ascii_lowercase()),
            ctrl: false,
            alt: false,
            shift: c.is_uppercase(),
            text: Some(c.to_string()),
        }
    }

    /// Builds a bare `Enter` press (newline under defaults).
    #[must_use]
    pub fn enter() -> Self {
        Self {
            key: ComposerKey::Enter,
            ctrl: false,
            alt: false,
            shift: false,
            text: None,
        }
    }

    /// Builds a `Ctrl+Enter` press (submit under defaults).
    #[must_use]
    pub fn ctrl_enter() -> Self {
        Self {
            key: ComposerKey::Enter,
            ctrl: true,
            alt: false,
            shift: false,
            text: None,
        }
    }

    /// Builds an `Esc` press (close under defaults).
    #[must_use]
    pub fn escape() -> Self {
        Self {
            key: ComposerKey::Escape,
            ctrl: false,
            alt: false,
            shift: false,
            text: None,
        }
    }

    /// Builds an `Alt+E` press (external editor under defaults).
    #[must_use]
    pub fn alt_e() -> Self {
        Self {
            key: ComposerKey::Char('e'),
            ctrl: false,
            alt: true,
            shift: false,
            text: None,
        }
    }
}

/// What one [`ComposerSession::feed`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerFeedOutcome {
    /// Printable text appended to the buffer.
    Inserted,
    /// Newline appended.
    Newline,
    /// Submit frame ready for a single PTY write (session auto-closes,
    /// buffer cleared after framing).
    Submitted(Vec<u8>),
    /// Composer closed, buffer preserved for reopen.
    Closed,
    /// Caller should run [`edit_externally`] then feed the result back via
    /// [`ComposerSession::apply_external_result`].
    ExternalEditorRequested,
    /// Press matched no composer role while open (e.g. function key);
    /// swallowed, buffer untouched, session stays open.
    Ignored,
}

/// Why a feed failed (all fail-closed: buffer and open-state untouched,
///
/// except `NotOpen`, which by definition changes nothing either).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerFeedError {
    /// Feed called while closed (bytes belong to the shell/PTY).
    NotOpen,
    /// Buffer cap hit while inserting.
    TooLarge {
        /// Bytes the content would have had.
        wanted: usize,
    },
}

impl std::fmt::Display for ComposerFeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOpen => f.write_str("composer is not open"),
            Self::TooLarge { wanted } => {
                write!(
                    f,
                    "composer buffer too large ({wanted} bytes, max {COMPOSER_MAX_BYTES})"
                )
            }
        }
    }
}

impl std::error::Error for ComposerFeedError {}

impl From<BufferError> for ComposerFeedError {
    fn from(value: BufferError) -> Self {
        match value {
            BufferError::TooLarge { wanted } => Self::TooLarge { wanted },
        }
    }
}

/// Open/close routing around one [`CommandBuffer`]: the hard boundary
/// (008 §14) made explicit.
///
/// While closed (the default), [`feed`](Self::feed) fails closed with
/// [`ComposerFeedError::NotOpen`] and the caller must send bytes to the PTY
/// unchanged (see [`normal_mode_passthrough`]). While open, composer keys
/// route to the buffer/submit/close/editor and never to the PTY. Opening is
/// always explicit ([`open`](Self::open)); there is no auto-enter path in
/// this slice ([`should_auto_offer`] is hard `false`).
#[derive(Debug, Clone)]
pub struct ComposerSession {
    buffer: CommandBuffer,
    keys: ComposerKeys,
    open: bool,
}

impl Default for ComposerSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ComposerSession {
    /// Closed session with an empty buffer and default keys.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: CommandBuffer::new(),
            keys: ComposerKeys::default(),
            open: false,
        }
    }

    /// Closed session with custom keys.
    #[must_use]
    pub fn with_keys(keys: ComposerKeys) -> Self {
        Self {
            buffer: CommandBuffer::new(),
            keys,
            open: false,
        }
    }

    /// Explicitly opens the composer (manual open only; no auto-enter).
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Closes the composer, preserving the buffer for reopen.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether composer keys currently route to [`feed`](Self::feed).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Current buffer content.
    #[must_use]
    pub fn content(&self) -> &str {
        self.buffer.as_str()
    }

    /// Active key bindings.
    #[must_use]
    pub fn keys(&self) -> ComposerKeys {
        self.keys
    }

    /// Replaces the key bindings (only meaningful while closed or open;
    /// never touches the buffer).
    pub fn set_keys(&mut self, keys: ComposerKeys) {
        self.keys = keys;
    }

    /// Feeds one key press while open.
    ///
    /// Role precedence when a press matches several roles is impossible by
    /// construction ([`ComposerKeys::validate`] forces distinct chords), so
    /// matching order is submit → close → external-editor → newline →
    /// printable text → ignored.
    ///
    /// # Errors
    /// [`ComposerFeedError::NotOpen`] while closed;
    /// [`ComposerFeedError::TooLarge`] when an insert would exceed the cap
    /// (buffer and open-state untouched).
    pub fn feed(&mut self, ev: ComposerKeyEvent) -> Result<ComposerFeedOutcome, ComposerFeedError> {
        if !self.open {
            return Err(ComposerFeedError::NotOpen);
        }
        let probe = ComposerKeyEvent {
            text: None,
            ..ev.clone()
        };
        if self.keys.submit.matches_event(probe.clone()) {
            let frame = frame_submit(self.buffer.as_str()).map_err(|e| match e {
                BufferError::TooLarge { wanted } => ComposerFeedError::TooLarge { wanted },
            })?;
            self.buffer.clear();
            self.open = false;
            return Ok(ComposerFeedOutcome::Submitted(frame));
        }
        if self.keys.close.matches_event(probe.clone()) {
            self.open = false;
            return Ok(ComposerFeedOutcome::Closed);
        }
        if self.keys.external_editor.matches_event(probe.clone()) {
            return Ok(ComposerFeedOutcome::ExternalEditorRequested);
        }
        if self.keys.newline.matches_event(probe) {
            self.buffer.push_newline()?;
            return Ok(ComposerFeedOutcome::Newline);
        }
        if ev.ctrl || ev.alt {
            // Modifier press with printable text is not buffer input (it is
            // either a role above — already checked — or a chrome chord the
            // caller owns). Swallow without touching the buffer.
            return Ok(ComposerFeedOutcome::Ignored);
        }
        if let Some(text) = ev.text {
            if text.is_empty() {
                return Ok(ComposerFeedOutcome::Ignored);
            }
            self.buffer.insert_str(&text)?;
            return Ok(ComposerFeedOutcome::Inserted);
        }
        Ok(ComposerFeedOutcome::Ignored)
    }

    /// Applies an external-editor round-trip result back to the buffer
    /// (fails closed past the cap, old content kept).
    ///
    /// # Errors
    /// [`ComposerFeedError::TooLarge`] when `content` exceeds the cap.
    pub fn apply_external_result(&mut self, content: &str) -> Result<(), ComposerFeedError> {
        self.buffer.set(content).map_err(ComposerFeedError::from)
    }
}

/// Normal-Mode passthrough: bytes that must reach the PTY unchanged.
///
/// While the composer is closed, input routing calls this (identity) and
/// writes the result to the PTY. It exists so the boundary is a named,
/// tested function rather than an implicit `else` branch: any future
/// auto-offer must go through explicit open, never by editing this path.
#[must_use]
pub fn normal_mode_passthrough(bytes: &[u8]) -> Vec<u8> {
    bytes.to_vec()
}

/// OSC-133 prompt-aware auto-offer gate: always `false` in this slice.
///
/// `has_input_phase` reports whether shell integration currently sees an
/// `InputStart` zone; the composer ignores it on purpose (fail-open
/// default: manual open only). A follow-up slice may introduce an explicit
/// opt-in `auto = true` config that consults this signal — until then this
/// function pins the boundary and any caller wiring auto-enter is a bug.
#[must_use]
pub fn should_auto_offer(_has_input_phase: bool) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Submit framing: CommandBuffer → bracketed paste → shell → final Enter
// ---------------------------------------------------------------------------

/// Frames `content` as one bracketed-paste submit frame:
///
/// `ESC[200~` + `content` + `ESC[201~` + `CR`
///
/// The caller writes the returned bytes to the PTY in a single write; the
/// shell line editor receives the content paste-safely (Unicode/multiline
/// intact) followed by the final `Enter`. No keystroke simulation: content
/// bytes are never re-encoded as individual key presses.
///
/// # Errors
/// [`BufferError::TooLarge`] when the framed output would exceed the
/// buffer cap plus framing overhead (nothing is emitted).
pub fn frame_submit(content: &str) -> Result<Vec<u8>, BufferError> {
    let wanted = content
        .len()
        .saturating_add(PASTE_OPEN.len())
        .saturating_add(PASTE_CLOSE.len())
        .saturating_add(SUBMIT_TERMINATOR.len());
    if content.len() > COMPOSER_MAX_BYTES || wanted > COMPOSER_MAX_BYTES.saturating_add(16) {
        return Err(BufferError::TooLarge { wanted });
    }
    let mut out = Vec::with_capacity(wanted);
    out.extend_from_slice(PASTE_OPEN.as_bytes());
    out.extend_from_slice(content.as_bytes());
    out.extend_from_slice(PASTE_CLOSE.as_bytes());
    out.extend_from_slice(SUBMIT_TERMINATOR.as_bytes());
    Ok(out)
}

// ---------------------------------------------------------------------------
// External editor round-trip ($VISUAL / $EDITOR, secure temp file)
// ---------------------------------------------------------------------------

/// Picks the editor program: `$VISUAL`, else `$EDITOR` (trimmed, non-empty).
///
/// Returns `None` when neither is set — the caller fails closed (composer
/// content preserved) instead of guessing `vi`.
#[must_use]
pub fn resolve_editor(visual: Option<&str>, editor: Option<&str>) -> Option<String> {
    for raw in [visual, editor].into_iter().flatten() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

/// Why the external-editor round-trip failed.
///
/// Every variant is fail-closed: the composer buffer is untouched and the
/// temp file is deleted. No variant leaks the temp path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    /// Neither `$VISUAL` nor `$EDITOR` is set.
    NoEditor,
    /// Temp file could not be created/written.
    WriteFailed(String),
    /// Editor could not be spawned.
    SpawnFailed(String),
    /// Editor did not exit within `timeout` (killed).
    Timeout,
    /// Editor exited non-zero (echoes the code when known).
    NonZeroExit(Option<i32>),
    /// Edited file could not be read back.
    ReadFailed(String),
    /// Edited file exceeds [`COMPOSER_MAX_BYTES`].
    TooLarge {
        /// Bytes observed on disk.
        wanted: usize,
    },
    /// Edited file is not valid UTF-8 (lossy read-back is refused).
    InvalidUtf8,
    /// Interrupted while waiting (underlying wait error).
    WaitFailed(String),
}

impl std::fmt::Display for EditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoEditor => f.write_str("no editor: set $VISUAL or $EDITOR"),
            Self::WriteFailed(e) => write!(f, "composer temp file write failed: {e}"),
            Self::SpawnFailed(e) => write!(f, "editor spawn failed: {e}"),
            Self::Timeout => write!(f, "editor timed out and was killed"),
            Self::NonZeroExit(code) => match code {
                Some(c) => write!(f, "editor exited with status {c}"),
                None => f.write_str("editor exited with unknown failure"),
            },
            Self::ReadFailed(e) => write!(f, "composer temp file read failed: {e}"),
            Self::TooLarge { wanted } => {
                write!(
                    f,
                    "edited file too large ({wanted} bytes, max {COMPOSER_MAX_BYTES})"
                )
            }
            Self::InvalidUtf8 => f.write_str("edited file is not valid UTF-8"),
            Self::WaitFailed(e) => write!(f, "editor wait failed: {e}"),
        }
    }
}

impl std::error::Error for EditorError {}

/// RAII guard for the composer temp file: deleted on drop (best-effort).
///
/// The file is created `0600` on Unix (owner-only) at construction. `Drop`
/// unlinks it even when the editor fails, times out, or the caller panics
/// past this frame — cleanup is structural, not a `finally` the caller can
/// forget.
#[derive(Debug)]
pub struct TempComposerFile {
    path: PathBuf,
}

impl TempComposerFile {
    /// Temp path (for the editor child arg and read-back only).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempComposerFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_temp_path(dir: &Path) -> PathBuf {
    let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "{TEMP_PREFIX}{}-{}-{seq}.sh",
        std::process::id(),
        nanos
    ))
}

/// Writes `content` to a fresh `0600` temp file under `dir`.
///
/// The file is created with `create_new` (never overwrites) and restricted
/// to owner-only on Unix before any content lands. Size is bounded at
/// [`COMPOSER_MAX_BYTES`].
///
/// # Errors
/// [`EditorError::TooLarge`] when `content` exceeds the cap;
/// [`EditorError::WriteFailed`] on any filesystem failure.
pub fn write_composer_temp(content: &str, dir: &Path) -> Result<TempComposerFile, EditorError> {
    if content.len() > COMPOSER_MAX_BYTES {
        return Err(EditorError::TooLarge {
            wanted: content.len(),
        });
    }
    // Bounded attempts: uniqueness comes from pid+nanos+counter, so one
    // collision retry loop capped at 8 tries cannot spin.
    let mut last_err = String::from("no attempt");
    for _ in 0..8 {
        let path = unique_temp_path(dir);
        let open = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match open {
            Ok(mut file) => {
                restrict_owner_only(&path);
                if let Err(e) = file.write_all(content.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(EditorError::WriteFailed(truncate_err(e.to_string())));
                }
                if let Err(e) = file.flush() {
                    let _ = std::fs::remove_file(&path);
                    return Err(EditorError::WriteFailed(truncate_err(e.to_string())));
                }
                drop(file);
                restrict_owner_only(&path);
                return Ok(TempComposerFile { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = e.to_string();
                continue;
            }
            Err(e) => return Err(EditorError::WriteFailed(truncate_err(e.to_string()))),
        }
    }
    Err(EditorError::WriteFailed(truncate_err(last_err)))
}

#[cfg(unix)]
fn restrict_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let perm = std::fs::Permissions::from_mode(0o600);
    let _ = std::fs::set_permissions(path, perm);
}

#[cfg(not(unix))]
fn restrict_owner_only(_path: &Path) {
    // Windows ACL owner-only restriction needs platform APIs; temp-dir
    // placement plus immediate unlink is the documented best-effort there.
}

fn truncate_err(mut s: String) -> String {
    if s.len() > 256 {
        s.truncate(256);
    }
    s
}

/// Spawns `editor` with the temp path as its only argument and waits up to
/// `timeout` (clamped to [`EDITOR_TIMEOUT_MAX`).
///
/// No shell: the program string is passed to `Command::new` unsplit, and
/// the temp path travels as one argv element, so spaces in either are data.
/// On timeout the child is killed and [`EditorError::Timeout`] is returned.
///
/// # Errors
/// [`EditorError::SpawnFailed`], [`EditorError::Timeout`],
/// [`EditorError::NonZeroExit`], [`EditorError::WaitFailed`].
pub fn run_editor(editor: &str, path: &Path, timeout: Duration) -> Result<(), EditorError> {
    let program = editor.trim();
    if program.is_empty() {
        return Err(EditorError::NoEditor);
    }
    let timeout = timeout.min(EDITOR_TIMEOUT_MAX);
    let mut child = std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| EditorError::SpawnFailed(truncate_err(e.to_string())))?;
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                return Err(EditorError::NonZeroExit(status.code()));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(EditorError::Timeout);
                }
                std::thread::sleep(EDITOR_POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(EditorError::WaitFailed(truncate_err(e.to_string())));
            }
        }
    }
}

/// Reads the edited temp file back, bounded at [`COMPOSER_MAX_BYTES`].
///
/// # Errors
/// [`EditorError::ReadFailed`] on I/O failure;
/// [`EditorError::TooLarge`] past the cap;
/// [`EditorError::InvalidUtf8`] when the file is not valid UTF-8.
pub fn read_composer_back(path: &Path) -> Result<String, EditorError> {
    let bytes =
        std::fs::read(path).map_err(|e| EditorError::ReadFailed(truncate_err(e.to_string())))?;
    if bytes.len() > COMPOSER_MAX_BYTES {
        return Err(EditorError::TooLarge {
            wanted: bytes.len(),
        });
    }
    String::from_utf8(bytes).map_err(|_| EditorError::InvalidUtf8)
}

/// Full external-editor round-trip against `buffer`:
///
/// 1. resolves `$VISUAL`/`$EDITOR` ([`resolve_editor`], fail-closed),
/// 2. writes a `0600` temp file under `dir` (or [`std::env::temp_dir`]
///    when `None`),
/// 3. spawns the editor with a bounded timeout + kill,
/// 4. reads the file back (bounded, UTF-8),
/// 5. deletes the temp file in all cases (RAII),
/// 6. installs the result into `buffer` (fail-closed past the cap, old
///    content kept).
///
/// Returns the new content on success. On any [`EditorError`] the buffer is
/// untouched and the temp file is still gone.
///
/// # Errors
/// Any [`EditorError`]; `buffer` is preserved on every error path.
pub fn edit_externally(
    buffer: &mut CommandBuffer,
    visual: Option<&str>,
    editor: Option<&str>,
    timeout: Duration,
    dir: Option<&Path>,
) -> Result<String, EditorError> {
    let program = resolve_editor(visual, editor).ok_or(EditorError::NoEditor)?;
    let owned_dir;
    let dir: &Path = match dir {
        Some(d) => d,
        None => {
            owned_dir = std::env::temp_dir();
            &owned_dir
        }
    };
    let temp = write_composer_temp(buffer.as_str(), dir)?;
    let temp_path = temp.path().to_path_buf();
    let run = run_editor(&program, &temp_path, timeout);
    // Read back only when the editor succeeded; every path drops `temp`
    // (deleting the file) before returning.
    match run {
        Ok(()) => {
            let content = read_composer_back(&temp_path)?;
            // `set` enforces the same cap; map to the editor error type.
            match buffer.set(&content) {
                Ok(()) => {
                    drop(temp);
                    Ok(content)
                }
                Err(BufferError::TooLarge { wanted }) => {
                    drop(temp);
                    Err(EditorError::TooLarge { wanted })
                }
            }
        }
        Err(e) => {
            drop(temp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workdir() -> PathBuf {
        // Unique subdir per call: tests run in parallel in one process, so
        // a shared dir would let one test observe another's live temp file.
        let seq = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("bitty-composer-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&base).expect("test workdir");
        base
    }

    // -- buffer editing semantics ------------------------------------------

    #[test]
    fn buffer_insert_newline_and_clear() {
        let mut buf = CommandBuffer::new();
        assert!(buf.is_empty());
        buf.insert_str("docker run --rm \\").expect("insert");
        buf.push_newline().expect("newline");
        buf.insert_str("  ubuntu:latest bash").expect("insert");
        assert_eq!(buf.as_str(), "docker run --rm \\\n  ubuntu:latest bash");
        assert_eq!(buf.len_bytes(), buf.as_str().len());
        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_unicode_multiline_round_trip() {
        let mut buf = CommandBuffer::new();
        buf.insert_str("echo 'héllo世界🦀'").expect("unicode");
        buf.push_newline().expect("newline");
        buf.push_char('ñ').expect("char");
        assert!(buf.as_str().contains("héllo世界🦀"));
        assert!(buf.as_str().contains('ñ'));
    }

    #[test]
    fn buffer_cap_fails_closed_and_keeps_content() {
        let mut buf = CommandBuffer::with_content("keep me").expect("seed");
        let big = "x".repeat(COMPOSER_MAX_BYTES);
        let err = buf.insert_str(&big).expect_err("over cap");
        assert!(matches!(err, BufferError::TooLarge { .. }));
        assert_eq!(buf.as_str(), "keep me");
        let err = CommandBuffer::with_content(&big.repeat(2)).expect_err("ctor over cap");
        assert!(matches!(err, BufferError::TooLarge { .. }));
    }

    #[test]
    fn buffer_set_over_cap_keeps_old_content() {
        let mut buf = CommandBuffer::with_content("original").expect("seed");
        let big = "y".repeat(COMPOSER_MAX_BYTES + 1);
        assert!(buf.set(&big).is_err());
        assert_eq!(buf.as_str(), "original");
    }

    // -- keys: defaults, customization, distinctness ------------------------

    #[test]
    fn keys_default_match_spec() {
        let keys = ComposerKeys::default();
        assert_eq!(keys.newline.canonical(), "enter");
        assert_eq!(keys.submit.canonical(), "ctrl+enter");
        assert_eq!(keys.close.canonical(), "escape");
        assert_eq!(keys.external_editor.canonical(), "alt+e");
    }

    #[test]
    fn keys_custom_submit_enter_newline_shift_enter() {
        let keys = ComposerKeys::from_chords("Shift+Enter", "Enter", "Escape", "Alt+E")
            .expect("custom keys");
        assert_eq!(keys.newline.canonical(), "shift+enter");
        assert_eq!(keys.submit.canonical(), "enter");
    }

    #[test]
    fn keys_duplicate_roles_fail_closed() {
        let err = ComposerKeys::from_chords("Enter", "Enter", "Escape", "Alt+E").expect_err("dup");
        assert!(matches!(err, ComposerKeysError::Duplicate(_)));
    }

    #[test]
    fn keys_bad_chord_fails_closed() {
        assert!(ComposerKeys::from_chords("", "Enter", "Escape", "Alt+E").is_err());
    }

    // -- open chord gate -----------------------------------------------------

    #[test]
    fn open_chord_accepts_alt_e_and_rejects_bare_e() {
        let open = validate_open_chord("alt+e").expect("alt+e opens");
        assert_eq!(open.key, 'e');
        assert!(open.alt);
        let err = validate_open_chord("e").expect_err("bare e must fail");
        assert_eq!(err, OpenChordError::BareKey('e'));
        assert!(validate_open_chord("").is_err());
        assert!(validate_open_chord("ctrl+alt+e").is_ok());
    }

    #[test]
    fn open_chord_default_constant_validates() {
        assert!(validate_open_chord(COMPOSER_OPEN_CHORD).is_ok());
    }

    // -- session routing + hard boundary -------------------------------------

    #[test]
    fn closed_session_feed_fails_and_normal_bytes_pass_through() {
        let mut session = ComposerSession::new();
        assert!(!session.is_open());
        let err = session
            .feed(ComposerKeyEvent::enter())
            .expect_err("closed feed fails");
        assert_eq!(err, ComposerFeedError::NotOpen);
        // Normal-Mode Enter is the VT CR byte, identical before/after.
        let raw = b"\r";
        assert_eq!(normal_mode_passthrough(raw), raw.to_vec());
        let raw = "echo héllo\n".as_bytes();
        assert_eq!(normal_mode_passthrough(raw), raw.to_vec());
    }

    #[test]
    fn open_session_enter_newline_ctrl_enter_submits_and_closes() {
        let mut session = ComposerSession::new();
        session.open();
        assert!(session.feed(ComposerKeyEvent::printable('a')).is_ok());
        assert_eq!(
            session.feed(ComposerKeyEvent::enter()).expect("newline"),
            ComposerFeedOutcome::Newline
        );
        assert_eq!(session.content(), "a\n");
        match session
            .feed(ComposerKeyEvent::ctrl_enter())
            .expect("submit")
        {
            ComposerFeedOutcome::Submitted(bytes) => {
                assert!(bytes.starts_with(PASTE_OPEN.as_bytes()));
                assert!(bytes.ends_with(b"\x1b[201~\r"));
                assert!(session.content().is_empty());
                assert!(!session.is_open());
            }
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn escape_closes_and_preserves_buffer_for_reopen() {
        let mut session = ComposerSession::new();
        session.open();
        session
            .feed(ComposerKeyEvent::printable('x'))
            .expect("insert");
        assert_eq!(
            session.feed(ComposerKeyEvent::escape()).expect("close"),
            ComposerFeedOutcome::Closed
        );
        assert!(!session.is_open());
        assert_eq!(session.content(), "x");
        session.open();
        assert_eq!(session.content(), "x");
    }

    #[test]
    fn alt_e_requests_external_editor_without_touching_buffer() {
        let mut session = ComposerSession::new();
        session.open();
        session
            .feed(ComposerKeyEvent::printable('q'))
            .expect("insert");
        assert_eq!(
            session.feed(ComposerKeyEvent::alt_e()).expect("editor"),
            ComposerFeedOutcome::ExternalEditorRequested
        );
        assert!(session.is_open());
        assert_eq!(session.content(), "q");
    }

    #[test]
    fn session_insert_cap_fails_closed_and_stays_open() {
        let mut session = ComposerSession::new();
        session.open();
        let big = "z".repeat(COMPOSER_MAX_BYTES + 1);
        let ev = ComposerKeyEvent {
            key: ComposerKey::Char('z'),
            ctrl: false,
            alt: false,
            shift: false,
            text: Some(big),
        };
        assert!(matches!(
            session.feed(ev),
            Err(ComposerFeedError::TooLarge { .. })
        ));
        assert!(session.is_open());
        assert!(session.content().is_empty());
    }

    #[test]
    fn auto_offer_is_always_manual_only() {
        assert!(!should_auto_offer(false));
        assert!(!should_auto_offer(true));
    }

    // -- submit framing exact -----------------------------------------------

    #[test]
    fn submit_framing_markers_exact() {
        let frame = frame_submit("cargo test").expect("frame");
        assert_eq!(frame, b"\x1b[200~cargo test\x1b[201~\r".to_vec());
    }

    #[test]
    fn submit_framing_empty_buffer() {
        assert_eq!(
            frame_submit("").expect("frame"),
            b"\x1b[200~\x1b[201~\r".to_vec()
        );
    }

    #[test]
    fn submit_framing_unicode_and_multiline() {
        let content = "echo 'héllo世界🦀'\nls -la";
        let frame = frame_submit(content).expect("frame");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x1b[200~");
        expected.extend_from_slice(content.as_bytes());
        expected.extend_from_slice(b"\x1b[201~\r");
        assert_eq!(frame, expected);
    }

    #[test]
    fn submit_framing_over_cap_emits_nothing() {
        let big = "q".repeat(COMPOSER_MAX_BYTES + 1);
        assert!(frame_submit(&big).is_err());
    }

    // -- editor resolution ----------------------------------------------------

    #[test]
    fn resolve_editor_prefers_visual_then_editor() {
        assert_eq!(
            resolve_editor(Some("nvim"), Some("vim")),
            Some("nvim".to_string())
        );
        assert_eq!(resolve_editor(None, Some("vim")), Some("vim".to_string()));
        assert_eq!(
            resolve_editor(Some("  "), Some("vim")),
            Some("vim".to_string())
        );
        assert_eq!(resolve_editor(None, None), None);
        assert_eq!(resolve_editor(Some(""), Some("  ")), None);
    }

    // -- editor round-trip with fake editor script ----------------------------

    // POSIX-only: fake editors are `#!/bin/sh` scripts executed directly.
    // Windows has no `/bin/sh`, so `Command::new(*.sh)` fails with
    // SpawnFailed. Gate helper + tests on unix (PX-1232).
    #[cfg(unix)]
    fn fake_editor_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(format!(
            "fake-editor-{}-{}.sh",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // fsync before close: executing a just-written script can hit
        // ETXTBSY under parallel `cargo test` load without it.
        let mut file = std::fs::File::create(&path).expect("create fake editor");
        std::io::Write::write_all(&mut file, body.as_bytes()).expect("write fake editor");
        file.sync_all().expect("fsync fake editor");
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        path
    }

    /// Runs `edit_externally`, retrying transient ETXTBSY spawns.
    ///
    /// Under parallel `cargo test` load the kernel can refuse `execve` of a
    /// just-written fake editor with "Text file busy (os error 26)" even
    /// after fsync+close. That is an environment race, not a product
    /// failure, so retry the spawn a bounded number of times; every other
    /// error (including `Timeout` and `NonZeroExit`) returns immediately
    /// and keeps its deterministic assertion value.
    #[cfg(unix)]
    fn edit_with_spawn_retry(
        buf: &mut CommandBuffer,
        editor: &str,
        timeout: Duration,
        dir: &Path,
    ) -> Result<String, EditorError> {
        let mut last: Option<EditorError> = None;
        for _ in 0..20 {
            match edit_externally(buf, None, Some(editor), timeout, Some(dir)) {
                Ok(out) => return Ok(out),
                Err(EditorError::SpawnFailed(msg)) if msg.contains("Text file busy") => {
                    last = Some(EditorError::SpawnFailed(msg));
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(other) => return Err(other),
            }
        }
        Err(last.expect("retry loop always sets last on ETXTBSY"))
    }

    #[test]
    #[cfg(unix)]
    fn editor_round_trip_with_fake_editor() {
        let dir = workdir();
        let script = fake_editor_script(
            &dir,
            "#!/bin/sh\nprintf 'edited content\\nline2' > \"$1\"\n",
        );
        let mut buf = CommandBuffer::with_content("original").expect("seed");
        let out = edit_with_spawn_retry(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_secs(10),
            &dir,
        )
        .expect("round trip");
        assert_eq!(out, "edited content\nline2");
        assert_eq!(buf.as_str(), "edited content\nline2");
        // Temp file is gone: no bitty-composer leftovers in the dir.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp file cleaned up");
    }

    #[test]
    #[cfg(unix)]
    fn editor_failure_preserves_buffer_and_still_cleans_temp() {
        let dir = workdir();
        // Failing editor: exits 1 without touching the file.
        let script = fake_editor_script(&dir, "#!/bin/sh\nexit 1\n");
        let mut buf = CommandBuffer::with_content("precious").expect("seed");
        let err = edit_with_spawn_retry(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_secs(10),
            &dir,
        )
        .expect_err("must fail");
        assert!(matches!(err, EditorError::NonZeroExit(Some(1))));
        assert_eq!(buf.as_str(), "precious");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp file cleaned up on failure");
    }

    #[test]
    fn editor_missing_fails_closed_without_temp_side_effects() {
        let dir = workdir();
        let before: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .collect();
        let mut buf = CommandBuffer::with_content("untouched").expect("seed");
        let err = edit_externally(&mut buf, None, None, Duration::from_secs(5), Some(&dir))
            .expect_err("no editor");
        assert_eq!(err, EditorError::NoEditor);
        assert_eq!(buf.as_str(), "untouched");
        let after: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(before.len(), after.len());
    }

    #[test]
    fn temp_guard_deletes_on_drop_even_without_editor() {
        let dir = workdir();
        let path: PathBuf;
        {
            let temp = write_composer_temp("scratch", &dir).expect("write temp");
            path = temp.path().to_path_buf();
            assert!(path.exists());
        }
        assert!(!path.exists(), "guard deleted the temp file");
    }

    #[test]
    fn temp_file_is_owner_only_on_unix() {
        let dir = workdir();
        let temp = write_composer_temp("secret", &dir).expect("write temp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(temp.path())
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(not(unix))]
        {
            assert!(temp.path().exists());
        }
    }

    #[test]
    #[cfg(unix)]
    fn editor_timeout_kills_and_preserves_buffer() {
        let dir = workdir();
        // Sleeper editor: sleeps 30 s; we allow 200 ms.
        let script = fake_editor_script(&dir, "#!/bin/sh\nsleep 30\n");
        let mut buf = CommandBuffer::with_content("waiting").expect("seed");
        let err = edit_with_spawn_retry(
            &mut buf,
            &script.to_string_lossy(),
            Duration::from_millis(200),
            &dir,
        )
        .expect_err("must time out");
        assert_eq!(err, EditorError::Timeout);
        assert_eq!(buf.as_str(), "waiting");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(TEMP_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "temp file cleaned up on timeout");
    }
}
