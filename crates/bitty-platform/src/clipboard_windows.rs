//! Bounded Windows text acquisition (CTX-1366 path B).
//!
//! The locked `arboard 3.6.1` Windows read calls `GetClipboardData` and then
//! copies/converts the whole `CF_UNICODETEXT` allocation before Bitty's
//! [`CLIPBOARD_MAX_BYTES`](crate::clipboard::CLIPBOARD_MAX_BYTES) ceiling is
//! applied. This module bounds the acquisition instead:
//!
//! 1. open the clipboard with a bounded retry count (same behavior as the
//!    previous path, not a native-call deadline);
//! 2. query the `HGLOBAL` extent with `GlobalSize` **before** copying any
//!    bytes (`clipboard-win::raw::size`, which locks only to read the size);
//! 3. reject an extent above
//!    [`WINDOWS_MAX_ALLOCATION_BYTES`](crate::clipboard_limits::WINDOWS_MAX_ALLOCATION_BYTES)
//!    with a typed [`AcquisitionError::OverLimit`] and no copy;
//! 4. copy at most that bounded extent and scan it with
//!    [`decode_windows_utf16`](crate::clipboard_limits::decode_windows_utf16),
//!    stopping at the NUL terminator and refusing the first character that
//!    would exceed the decoded ceiling.
//!
//! The scan retains no more than `CLIPBOARD_MAX_BYTES + 2` UTF-16 code units
//! (`2 * (8192 + 2)` native bytes). Post-copy truncation is not the control:
//! the size gate rejects before the copy, and the decode rejects rather than
//! returning a partial value.
//!
//! Residual scope: `OpenClipboard`, `GetClipboardData`, and any on-demand
//! format conversion have no caller cancellation API. The bounded retry only
//! bounds Bitty's own loop; it cannot bound the native object already provided
//! by the clipboard owner/service. The owned copy, decoder workspace, and
//! returned string are bounded, and the borrowed clipboard handle is never
//! freed.

use std::time::Duration;

use clipboard_win::raw;

use crate::clipboard::BoundedText;
use crate::clipboard_limits::{
    AcquisitionError, AcquisitionLimits, WINDOWS_MAX_ALLOCATION_BYTES, decode_windows_utf16,
    windows_allocation_within_bound,
};

const STAGE_SIZE: &str = "windows-hglobal";
const STAGE_COPY: &str = "windows-hglobal-copy";

/// Open attempts before giving up (matches the previous retry behavior).
const OPEN_ATTEMPTS: usize = 5;
/// Pause between open attempts; bounds Bitty's own retry loop only.
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// `CF_UNICODETEXT` clipboard format identifier.
const CF_UNICODETEXT: u32 = 13;

/// One bounded regular-clipboard text read.
///
/// Windows has no primary selection, so only the regular clipboard is read.
pub(crate) fn try_read_windows_text() -> Result<BoundedText, AcquisitionError> {
    let _opened = ClipboardAccess::acquire()?;
    if !raw::is_format_avail(CF_UNICODETEXT) {
        return Err(AcquisitionError::Unavailable(String::from(
            "Windows clipboard contains no CF_UNICODETEXT value",
        )));
    }
    // `raw::size` wraps GetClipboardData + GlobalLock + GlobalSize +
    // GlobalUnlock. It reports the allocation extent, never the logical text
    // length, and copies no payload bytes.
    let size = raw::size(CF_UNICODETEXT)
        .map(|size| size.get())
        .ok_or_else(|| {
            AcquisitionError::Unavailable(String::from(
                "Windows clipboard text has no readable allocation size",
            ))
        })?;
    if !windows_allocation_within_bound(size) {
        return Err(AcquisitionError::OverLimit {
            stage: STAGE_SIZE,
            observed_len: size,
        });
    }
    let copy_len = size.min(WINDOWS_MAX_ALLOCATION_BYTES);
    let mut buffer = vec![0_u8; copy_len];
    let copied =
        raw::get(CF_UNICODETEXT, &mut buffer).map_err(|err| AcquisitionError::Transport {
            stage: STAGE_COPY,
            detail: err.to_string(),
        })?;
    buffer.truncate(copied);
    decode_windows_utf16(&buffer, AcquisitionLimits::production().max_bytes)
}

/// Owned clipboard access; closes the clipboard on every exit after a
/// successful open.
struct ClipboardAccess;

impl ClipboardAccess {
    fn acquire() -> Result<Self, AcquisitionError> {
        let mut last_error = None;
        for attempt in 0..OPEN_ATTEMPTS {
            match raw::open() {
                Ok(()) => return Ok(Self),
                Err(err) => {
                    last_error = Some(err.to_string());
                    if attempt + 1 < OPEN_ATTEMPTS {
                        std::thread::sleep(OPEN_RETRY_DELAY);
                    }
                }
            }
        }
        Err(AcquisitionError::Unavailable(format!(
            "Windows clipboard could not be opened: {}",
            last_error.unwrap_or_else(|| String::from("unknown error"))
        )))
    }
}

impl Drop for ClipboardAccess {
    fn drop(&mut self) {
        let _ = raw::close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_size_gate_uses_the_shared_derived_window() {
        assert!(windows_allocation_within_bound(
            WINDOWS_MAX_ALLOCATION_BYTES
        ));
        assert!(!windows_allocation_within_bound(
            WINDOWS_MAX_ALLOCATION_BYTES + 1
        ));
    }
}
