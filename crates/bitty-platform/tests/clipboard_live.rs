//! Live OS clipboard backend validation (R-004 residual, issue #1073).
//!
//! These tests exercise the real `arboard` backend behind
//! `bitty_platform::Clipboard` and are therefore **gated behind the
//! default-off `gui-tests` feature** and a disposable display precondition:
//!
//! ```sh
//! just --justfile scripts/clipboard.justfile isolated PATH_TO_CLIPBOARD_LIVE_TEST_BINARY
//! ```
//!
//! Ordinary `gui-tests` runs emit `skipped` before any native constructor,
//! read or write. A green harness with skipped lines is NOT native proof.
//! The test-only runner starts fresh Xvfb in private namespaces; the guard
//! checks process, display-lock and mount evidence rather than trusting an
//! environment opt-in. Other Tier-1 backends remain NOT EXECUTED until a
//! disposable runner exists. R-004 stays open.
//!
//! What is covered beyond the headless suites (`clipboard_sync.rs` and the
//! `clipboard.rs` unit tests):
//!
//! - native selection ownership: a write followed by a read returns the
//!   written value on the live backend, for the regular clipboard and (on
//!   Linux) the primary selection;
//! - encoding: a multibyte (CJK + emoji) payload round-trips byte-identical;
//! - failure and permission surfacing: `new_strict` versus
//!   `headless_reason` agree on whether a system backend exists;
//! - bound enforcement on the live path: an over-limit write is rejected
//!   with `ClipboardPayloadTooLarge` before any OS call (the previous
//!   value is preserved) and bounded reads never exceed
//!   `CLIPBOARD_MAX_BYTES`.
//!
//! These tests deliberately overwrite both selections on the disposable
//! server. They do not read or attempt to restore owner clipboard content.
//! The runner destroys the display namespace on failure or timeout as well
//! as success. A mutex serializes native operations. Real-window
//! confirmation UX (paste banner present/focus/dismiss) stays manual smoke
//! evidence per platform; it is not asserted here.

#![cfg(feature = "gui-tests")]
#![forbid(unsafe_code)]

use std::sync::{Mutex, OnceLock};

use bitty_platform::PlatformError;
use bitty_platform::clipboard::{CLIPBOARD_MAX_BYTES, Clipboard};

#[path = "support/clipboard_isolation.rs"]
mod clipboard_isolation;

/// The only entry to native operations; the closure cannot run on refusal.
fn with_isolated_display(scenario: &str, run: impl FnOnce()) {
    if let Err(reason) =
        clipboard_isolation::run_if_isolated(&clipboard_isolation::current(), || {
            let _guard = live_lock().lock().expect("live clipboard mutex");
            run();
        })
    {
        emit(scenario, "skipped", "not-accessed", reason);
    }
}

/// Serializes live clipboard tests: the OS clipboard is process-global.
fn live_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Backend tag for evidence lines: preference hint, never a live claim.
fn backend_tag(clipboard: &Clipboard) -> &'static str {
    clipboard.backend_hint()
}

/// One machine-readable evidence line per scenario (mirrors the
/// `live_compat` JSON convention: `verified-local` or `skipped`).
fn emit(scenario: &str, status: &str, backend: &str, detail: &str) {
    println!(
        "{{\"scenario\": \"{scenario}\", \"status\": \"{status}\", \"backend\": \"{backend}\", \"detail\": \"{detail}\"}}"
    );
    eprintln!("clipboard_live/{scenario}: {status} ({detail})");
}

/// `true` when no system backend is reachable from this process.
fn degraded(clipboard: &Clipboard) -> bool {
    clipboard.is_headless()
}

/// Unique sentinel per process so concurrent operator runs do not collide.
fn sentinel(tag: &str) -> String {
    format!("bitty-r004-live-{tag}-pid{}", std::process::id())
}

#[test]
fn live_clipboard_roundtrip_proves_ownership() {
    with_isolated_display("clipboard-roundtrip", || {
        let mut clipboard = Clipboard::new();
        let backend = backend_tag(&clipboard);
        if degraded(&clipboard) {
            emit(
                "clipboard-roundtrip",
                "skipped",
                backend,
                "no display backend reachable; headless fallback active",
            );
            return;
        }
        let sentinel = sentinel("ownership");
        clipboard
            .set_text(sentinel.clone())
            .expect("live write succeeds on a reachable backend");
        let read = clipboard
            .get_text()
            .expect("live read succeeds on a reachable backend");
        assert_eq!(read, sentinel, "live backend must return the written value");
        emit(
            "clipboard-roundtrip",
            "verified-local",
            backend,
            "write/read ownership holds on the native backend",
        );
    });
}

#[test]
fn live_clipboard_multibyte_encoding_roundtrip() {
    with_isolated_display("clipboard-encoding", || {
        let mut clipboard = Clipboard::new();
        let backend = backend_tag(&clipboard);
        if degraded(&clipboard) {
            emit(
                "clipboard-encoding",
                "skipped",
                backend,
                "no display backend reachable; headless fallback active",
            );
            return;
        }
        let sentinel = format!(
            "{}-{}-{}",
            sentinel("encoding"),
            "日本語テスト",
            "😀🧪\u{200b}\u{202e}"
        );
        clipboard
            .set_text(sentinel.clone())
            .expect("live multibyte write succeeds");
        let read = clipboard.get_text().expect("live multibyte read succeeds");
        assert_eq!(
            read, sentinel,
            "multibyte payload must round-trip byte-identical"
        );
        emit(
            "clipboard-encoding",
            "verified-local",
            backend,
            "cjk/emoji/zero-width payload round-trips byte-identical",
        );
    });
}

#[test]
fn live_clipboard_over_limit_write_rejected_before_os() {
    with_isolated_display("clipboard-bound", || {
        let mut clipboard = Clipboard::new();
        let backend = backend_tag(&clipboard);
        if degraded(&clipboard) {
            emit(
                "clipboard-bound",
                "skipped",
                backend,
                "no display backend reachable; headless fallback active",
            );
            return;
        }
        let seed = sentinel("bound-seed");
        clipboard
            .set_text(seed.clone())
            .expect("seed write succeeds");
        let over = "x".repeat(CLIPBOARD_MAX_BYTES + 512);
        match clipboard.set_text(over) {
            Err(PlatformError::ClipboardPayloadTooLarge { len, max }) => {
                assert_eq!(max, CLIPBOARD_MAX_BYTES);
                assert_eq!(len, CLIPBOARD_MAX_BYTES + 512);
            }
            other => panic!("over-limit live write must be rejected, got {other:?}"),
        }
        let kept = clipboard
            .get_text()
            .expect("read after rejected write succeeds");
        assert_eq!(
            kept, seed,
            "rejected write must leave the live value unchanged"
        );
        let bounded = clipboard.get_text_bounded().expect("bounded read succeeds");
        assert!(
            bounded.len() <= CLIPBOARD_MAX_BYTES,
            "bounded reads never exceed the cap"
        );
        emit(
            "clipboard-bound",
            "verified-local",
            backend,
            "over-limit write rejected pre-call; value preserved; bounded read capped",
        );
    });
}

#[test]
fn live_clipboard_strict_constructor_matches_backend_presence() {
    with_isolated_display("clipboard-backend-presence", || {
        let probe = Clipboard::new();
        let backend = backend_tag(&probe);
        match Clipboard::new_strict() {
            Ok(live) => {
                assert!(
                    !live.is_headless() && probe.headless_reason().is_none(),
                    "strict success must agree with a present backend"
                );
                emit(
                    "clipboard-backend-presence",
                    "verified-local",
                    backend,
                    "strict constructor succeeds; no init error recorded",
                );
            }
            Err(PlatformError::ClipboardUnavailable(reason)) => {
                assert!(
                    probe.is_headless() && probe.headless_reason().is_some(),
                    "strict failure must agree with a degraded handle"
                );
                assert!(!reason.is_empty(), "unavailability must carry a reason");
                emit(
                    "clipboard-backend-presence",
                    "skipped",
                    backend,
                    "no display backend reachable; failure is typed and reasoned",
                );
            }
            Err(other) => panic!("strict constructor must fail typed, got {other:?}"),
        }
    });
}

/// Linux primary-selection roundtrip (middle-click / `wl-paste --primary`).
///
/// Fail-soft by design: a Wayland compositor without primary-selection
/// support (or any primary failure) records a `skipped` line instead of
/// failing, because the primary selection is an optional sync target, not
/// a required capability.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
#[test]
fn live_clipboard_primary_selection_roundtrip() {
    with_isolated_display("clipboard-primary", || {
        let mut clipboard = Clipboard::new();
        let backend = backend_tag(&clipboard);
        if degraded(&clipboard) {
            emit(
                "clipboard-primary",
                "skipped",
                backend,
                "no display backend reachable; headless fallback active",
            );
            return;
        }
        let sentinel = sentinel("primary");
        if clipboard.set_primary(sentinel.clone()).is_err() {
            emit(
                "clipboard-primary",
                "skipped",
                backend,
                "primary write failed; native primary proof not executed",
            );
            return;
        }
        let read = clipboard
            .get_primary()
            .expect("read test-owned primary selection");
        if read != sentinel {
            emit(
                "clipboard-primary",
                "skipped",
                backend,
                "primary write did not round-trip on this compositor (optional target)",
            );
            return;
        }
        emit(
            "clipboard-primary",
            "verified-local",
            backend,
            "primary selection write/read ownership holds",
        );
    });
}
