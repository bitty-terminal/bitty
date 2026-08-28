//! Clipboard primitives via `arboard` with headless fallback.
//!
//! This module wraps `arboard::Clipboard` behind an owned `Clipboard` type
//! that never panics on headless machines. When a display server is absent
//! (`arboard::Clipboard::new` fails) or an operation fails, the clipboard
//! falls back to an in-memory buffer so `bitty-runtime` selection copy/paste
//! remains headless-testable (`cargo test` on CI without X11/Wayland).
//!
//! The `Clipboard` is owned per `Runtime` instance; callers may also thread a
//! `Clipboard::new_headless()` that never touches the system clipboard, so
//! tests stay deterministic even on machines that do have a display.
//!
//! Security: `arboard` clipboard access is gated behind this seam so the
//! runtime can enforce `clipboard.read` / `clipboard.write` separately from
//! the platform primitive. This file itself never grants ambient access.

#![forbid(unsafe_code)]

use crate::error::PlatformError;

/// Maximum bytes allowed for a clipboard payload (mirrors
/// `BoundedBytes::MAX_LEN` / `CLIPBOARD_MAX_PAYLOAD_BYTES` = 4096 plus a
/// small slack for paste verification). Enforced before `arboard` calls so
/// unbounded paste data cannot grow the heap without limit (T-01).
pub const CLIPBOARD_MAX_BYTES: usize = 8192;

/// Owned clipboard handle with headless fallback.
///
/// On construction the inner `arboard::Clipboard` is attempted. If that
/// fails (headless CI, missing display server, permission error) the handle
/// degrades to an in-memory buffer: `set_text`/`get_text` operate on the
/// buffer and never return an error for that reason, so headless CI stays
/// green. When a system clipboard is available, operations are forwarded to
/// `arboard` and the buffer is kept in sync so `get_text` after a failed
/// system read can still return the last `set_text` value.
pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
    headless_buf: String,
    headless_only: bool,
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clipboard")
            .field("is_headless", &self.is_headless())
            .field("headless_len", &self.headless_buf.len())
            .finish()
    }
}

impl Clipboard {
    /// Attempts to open the system clipboard, falling back to memory when
    /// no display server is present.
    ///
    /// This never returns an error: even when `arboard::Clipboard::new()`
    /// fails the returned handle is usable headlessly. Callers that need
    /// strict failure should use [`Self::new_strict`].
    #[must_use]
    pub fn new() -> Self {
        match arboard::Clipboard::new() {
            Ok(inner) => Self {
                inner: Some(inner),
                headless_buf: String::new(),
                headless_only: false,
            },
            Err(_) => Self {
                inner: None,
                headless_buf: String::new(),
                headless_only: false,
            },
        }
    }

    /// Like [`Self::new`] but fails openly when no display server exists.
    ///
    /// Exposed for callers that want to surface `ClipboardUnavailable` instead
    /// of degrading. The headless buffer is still initialized empty.
    pub fn new_strict() -> Result<Self, PlatformError> {
        match arboard::Clipboard::new() {
            Ok(inner) => Ok(Self {
                inner: Some(inner),
                headless_buf: String::new(),
                headless_only: false,
            }),
            Err(err) => Err(PlatformError::ClipboardUnavailable(err.to_string())),
        }
    }

    /// Forced headless clipboard that never touches the OS (deterministic
    /// for unit/integration tests, even on live desktops).
    #[must_use]
    pub fn new_headless() -> Self {
        Self {
            inner: None,
            headless_buf: String::new(),
            headless_only: true,
        }
    }

    /// Whether the clipboard is operating headlessly (no system handle).
    #[must_use]
    pub fn is_headless(&self) -> bool {
        self.inner.is_none() || self.headless_only
    }

    /// Current contents of the headless buffer (for tests).
    #[must_use]
    pub fn headless_contents(&self) -> &str {
        &self.headless_buf
    }

    /// Writes `text` to the clipboard, truncating to [`CLIPBOARD_MAX_BYTES`]
    /// before the system call (T-01). When headless, writes only to the
    /// buffer and returns `Ok`.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and `arboard::Clipboard::set_text`
    /// fails, the error is returned as `PlatformError::ClipboardOperation`
    /// but the headless buffer is still updated so future `get_text` remains
    /// possible. Callers that want best-effort should ignore the error and
    /// keep the buffer.
    pub fn set_text(&mut self, text: String) -> Result<(), PlatformError> {
        let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
        if self.headless_only {
            self.headless_buf = truncated;
            return Ok(());
        }
        if let Some(inner) = self.inner.as_mut() {
            match inner.set_text(truncated.clone()) {
                Ok(()) => {
                    self.headless_buf = truncated;
                    Ok(())
                }
                Err(err) => {
                    self.headless_buf = truncated;
                    Err(PlatformError::ClipboardOperation(err.to_string()))
                }
            }
        } else {
            self.headless_buf = truncated;
            Ok(())
        }
    }

    /// Best-effort write that never returns an error: updates the headless
    /// buffer and attempts the system clipboard, dropping any system error.
    pub fn set_text_lossy(&mut self, text: String) {
        let _ = self.set_text(text);
    }

    /// Reads text from the clipboard, truncating to [`CLIPBOARD_MAX_BYTES`]
    /// after the system call. When headless, returns the buffer contents.
    ///
    /// # Errors
    ///
    /// When a system clipboard is present and `arboard::Clipboard::get_text`
    /// fails, returns `PlatformError::ClipboardOperation`. Headless `get_text`
    /// never fails.
    pub fn get_text(&mut self) -> Result<String, PlatformError> {
        if self.headless_only {
            return Ok(self.headless_buf.clone());
        }
        if let Some(inner) = self.inner.as_mut() {
            match inner.get_text() {
                Ok(text) => {
                    let truncated = truncate_to_bytes(text, CLIPBOARD_MAX_BYTES);
                    self.headless_buf = truncated.clone();
                    Ok(truncated)
                }
                Err(err) => Err(PlatformError::ClipboardOperation(err.to_string())),
            }
        } else {
            Ok(self.headless_buf.clone())
        }
    }

    /// Best-effort read that returns `Ok` even on system error: falls back
    /// to the headless buffer.
    #[must_use]
    pub fn get_text_lossy(&mut self) -> String {
        self.get_text()
            .unwrap_or_else(|_| self.headless_buf.clone())
    }

    /// Clears both system and headless clipboard to empty string.
    pub fn clear(&mut self) {
        self.headless_buf.clear();
        if let Some(inner) = self.inner.as_mut() {
            let _ = inner.set_text(String::new());
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate_to_bytes(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_clipboard_roundtrip_is_deterministic() {
        let mut cb = Clipboard::new_headless();
        assert!(cb.is_headless());
        assert_eq!(cb.headless_contents(), "");
        cb.set_text("hello world".to_string())
            .expect("headless set must succeed");
        assert_eq!(cb.get_text().expect("headless get"), "hello world");
        assert_eq!(cb.headless_contents(), "hello world");
        cb.set_text("second".to_string()).expect("overwrite");
        assert_eq!(cb.get_text_lossy(), "second");
        cb.clear();
        assert_eq!(cb.get_text_lossy(), "");
    }

    #[test]
    fn headless_truncates_at_max_bytes_on_char_boundary() {
        let mut cb = Clipboard::new_headless();
        let long = "a".repeat(CLIPBOARD_MAX_BYTES + 100);
        cb.set_text(long).expect("headless set");
        assert_eq!(cb.headless_contents().len(), CLIPBOARD_MAX_BYTES);
        // 4-byte emoji boundary
        let emoji = "😀".repeat((CLIPBOARD_MAX_BYTES / 4) + 10);
        cb.set_text(emoji).expect("emoji set");
        assert!(cb.headless_contents().len() <= CLIPBOARD_MAX_BYTES);
        assert!(
            cb.headless_contents().len() % 4 == 0
                || cb.headless_contents().len() < CLIPBOARD_MAX_BYTES
        );
    }

    #[test]
    fn new_never_panics_even_without_display() {
        let _cb = Clipboard::new();
    }

    #[test]
    fn lossy_helpers_never_error() {
        let mut cb = Clipboard::new_headless();
        cb.set_text_lossy("lossy".to_string());
        assert_eq!(cb.get_text_lossy(), "lossy");
    }
}
