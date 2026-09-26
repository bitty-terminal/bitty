//! CTX-0367 headless IME contract tests.
//!
//! Proves the composition path without a display server or PTY:
//!
//! 1. `Ime::Preedit` updates presentation-only preedit state, bounded to
//!    `IME_PREEDIT_MAX_CHARS` scalars, and never mutates grid truth.
//! 2. `Ime::Commit` routes UTF-8 through the same bounded input path as
//!    typed text, exactly once, including winit's documented
//!    `Preedit("")`-then-`Commit` sequence.
//! 3. Raw key presses during an active composition are consumed (no double
//!    input); the keyboard frees again once the commit gesture ends.
//! 4. `Ime::Disabled` clears the overlay without PTY bytes.
//! 5. Commit truncation is bounded to 256 chars / 1024 bytes at a char
//!    boundary.
//! 6. The caret rect forwarded to the platform IME tracks the DPI-scaled
//!    cursor cell and stays inside the surface.
//!
//! CTX-0783 extends the set with fcitx-shaped preedit/commit sequences for
//! issue #1449 (a commit leaving a trailing space). The fixtures are
//! synthetic: they replay the *event ordering* of a Wayland `text-input-v3`
//! or X11 XIM input method, never captured personal input.
//!
//! All headless and deterministic (no wall clock, no PTY spawn, no display).

#![forbid(unsafe_code)]

use bitty_platform::{
    ImeEvent, KeyEvent, KeyLocation, LogicalKey, NamedKey, PhysicalSize, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{IME_COMMIT_ECHO_WINDOW, Runtime};

fn char_key(ch: &str) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: Some(ch.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn ime_event(event: ImeEvent) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::Ime(event),
    }
}

fn key_event(event: KeyEvent) -> PlatformEvent {
    PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind: WindowEventKind::KeyboardInput(event),
    }
}

/// Feeds one composed ASCII letter the way a Wayland `text-input-v3` input
/// method does: the input method updates the preedit from the same physical
/// key that also reaches `wl_keyboard`, so the runtime sees the preedit
/// first and the raw key second, for every keystroke.
fn feed_typing_key(rt: &mut Runtime, preedit: &str, ch: &str) {
    let _ = rt.handle_platform_event(ime_event(ImeEvent::Preedit(
        preedit.to_string(),
        Some((preedit.len(), preedit.len())),
    )));
    let _ = rt.handle_platform_event(key_event(char_key(ch)));
}

/// Replays an fcitx commit in the order winit emits it for one `done` event:
/// the preedit-clear first, then the commit. With `trailing_commit_key`, the
/// raw keyboard press of the same physical key follows, which is what a
/// compositor that does not filter the IME's key stream delivers (Wayland).
fn feed_commit(rt: &mut Runtime, committed: &str, trailing_commit_key: Option<KeyEvent>) {
    let _ = rt.handle_platform_event(ime_event(ImeEvent::Preedit(String::new(), None)));
    let _ = rt.handle_platform_event(ime_event(ImeEvent::Commit(committed.to_string())));
    if let Some(key) = trailing_commit_key {
        let _ = rt.handle_platform_event(key_event(key));
    }
}

#[test]
fn preedit_state_is_bounded_and_never_touches_grid_truth() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let before = rt.snapshot();

    // 300 hostile scalars must truncate to the 128-scalar preedit bound.
    let long: String = "你".repeat(300);
    rt.handle_ime_preedit(Some(long), Some(0));
    let preedit = rt.ime_preedit().expect("preedit must be set");
    assert_eq!(
        preedit.chars().count(),
        128,
        "preedit must truncate to IME_PREEDIT_MAX_CHARS at a char boundary"
    );

    assert!(rt.tick().is_some(), "preedit must force a present");
    let after = rt.snapshot();
    assert_eq!(
        before.cells, after.cells,
        "preedit must not mutate grid truth"
    );
    assert_eq!(before.width, after.width);
    assert_eq!(before.height, after.height);

    // The platform caret rect is armed for the focused cursor and in-bounds.
    let area = rt.ime_cursor_area().expect("focused caret must be armed");
    assert!(area.width >= 1 && area.height >= 1);
    assert!(area.x >= 0 && area.y >= 0);
    let extent = rt.surface_extent().expect("surface extent after build");
    assert!(area.x + area.width as i32 <= extent.width() as i32);
    assert!(area.y + area.height as i32 <= extent.height() as i32);
}

#[test]
fn empty_preedit_then_commit_inserts_exactly_once() {
    // winit's `Ime::Commit` doc guarantee: an empty `Preedit` arrives right
    // before the commit. The empty preedit clears the overlay and emits no
    // bytes; the commit is the single insertion.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("nihao".to_string()), Some(5));
    assert!(rt.ime_preedit().is_some());

    rt.handle_ime_preedit(None, None);
    assert!(rt.ime_preedit().is_none(), "empty preedit clears overlay");
    assert_eq!(
        rt.pending_input_len(),
        0,
        "empty preedit must not emit input bytes"
    );

    rt.handle_ime_commit("你好".to_string());
    assert_eq!(rt.pending_input(), "你好".as_bytes(), "commit UTF-8 bytes");
    assert_eq!(rt.drain_pending_input(), "你好".as_bytes());
    assert_eq!(
        rt.pending_input_len(),
        0,
        "commit must land exactly once (no double input)"
    );
    assert!(rt.ime_preedit().is_none(), "commit clears preedit");
}

#[test]
fn raw_keys_during_composition_do_not_double_input() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("n".to_string()), Some(1));

    // Platform quirk: the raw key for the composition reaches the key path.
    // It must be consumed by the composition guard, not inserted.
    assert!(rt.handle_key_event(char_key("n")).is_none());
    assert!(rt.handle_key_event_ref(&char_key("i")).is_none());
    assert_eq!(
        rt.pending_input_len(),
        0,
        "composition owns the keyboard: no raw Latin insertion"
    );

    // Commit delivers the composed text once.
    rt.handle_ime_preedit(None, None);
    rt.handle_ime_commit("\u{4f60}".to_string());
    assert_eq!(rt.pending_input(), "\u{4f60}".as_bytes());
    rt.drain_pending_input();

    // CTX-0783: the keyboard frees again only once the commit *gesture* ends,
    // not the instant the commit lands. The raw copy of the committing key is
    // still in flight, so it is absorbed; the keystroke after it is real
    // input. (Before CTX-0783 this asserted the pre-fix semantics, where the
    // very next press inserted — the #1449 trailing space.)
    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "the raw copy of the committing key is absorbed, not inserted"
    );
    assert_eq!(rt.pending_input_len(), 0, "absorbed key emits no bytes");
    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "the claim is one-shot: later keys are real input again"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn ime_disabled_clears_preedit_without_bytes() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("zhong".to_string()), Some(0));

    let exit = rt.handle_platform_event(ime_event(ImeEvent::Disabled));
    assert!(!exit, "IME Disabled must not request exit");
    assert!(rt.ime_preedit().is_none(), "Disabled clears the overlay");
    assert_eq!(rt.pending_input_len(), 0, "Disabled emits no bytes");

    // The full platform seam also routes preedit/commit identically.
    let exit =
        rt.handle_platform_event(ime_event(ImeEvent::Preedit("ni".to_string(), Some((0, 2)))));
    assert!(!exit);
    assert_eq!(rt.ime_preedit(), Some("ni"));
    let exit = rt.handle_platform_event(ime_event(ImeEvent::Commit("你".to_string())));
    assert!(!exit);
    assert_eq!(rt.pending_input(), "你".as_bytes());
    assert!(rt.ime_preedit().is_none());
}

#[test]
fn commit_is_bounded_256_chars_and_1024_bytes() {
    // 300 one-byte scalars: the character cap bites.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_commit("a".repeat(300));
    assert_eq!(rt.drain_pending_input().len(), 256);

    // 300 four-byte scalars: the byte cap bites first and stays valid UTF-8.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_commit("\u{20000}".repeat(300));
    let bytes = rt.drain_pending_input();
    assert!(bytes.len() <= 1024, "commit bytes bounded: {}", bytes.len());
    assert!(
        std::str::from_utf8(&bytes).is_ok(),
        "bounded commit must stay valid UTF-8"
    );
}

#[test]
fn preedit_cursor_is_char_indexed_against_hostile_byte_offsets() {
    // A cursor byte offset inside the second scalar of a 3-byte char must
    // snap down to a char boundary (never panic, never split).
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.handle_ime_preedit(Some("你好a".to_string()), Some(4));
    assert_eq!(rt.ime_preedit(), Some("你好a"));
    assert!(rt.tick().is_some(), "hostile cursor must still present");
}

#[test]
fn caret_area_scales_with_dpi() {
    let extent = PhysicalSize::new(1600, 1000);

    let mut base = Runtime::with_defaults().expect("headless runtime must build");
    base.handle_resize(extent).expect("resize");
    assert!(base.tick().is_some(), "base first present");
    let base_area = base.ime_cursor_area().expect("base caret");

    let mut hidpi = Runtime::with_defaults().expect("headless runtime must build");
    hidpi.handle_resize(extent).expect("resize");
    hidpi.apply_dpi_scale(2.0, Some(extent));
    assert!(hidpi.tick().is_some(), "hidpi first present");
    let hidpi_area = hidpi.ime_cursor_area().expect("hidpi caret");

    assert!(
        hidpi_area.width > base_area.width && hidpi_area.height > base_area.height,
        "DPI adoption must scale the caret cell: base={base_area:?} hidpi={hidpi_area:?}"
    );
}

// ---------------------------------------------------------------------------
// CTX-0783 / issue #1449 — the preedit-to-commit boundary.
//
// A Wayland `text-input-v3` input method and an X11 XIM client both receive
// one physical keystroke twice: once through the input-method protocol and
// once through the normal keyboard stream. winit's split point is what made
// the difference. While a preedit is up, winit pushes `Ime::Preedit` before
// the matching `KeyboardInput`, so the runtime's preedit guard suppressed the
// raw copy (CTX-0367). For the *committing* key, winit pushes the
// preedit-clear and `Ime::Commit` first, and the raw copy arrives after both
// with no preedit left to suppress it. Keying the guard on the preedit alone
// therefore re-inserted the raw copy of the committing key: an fcitx5 pinyin
// candidate selection is committed with `Space`, so the user saw the composed
// text followed by one extra space.
//
// The regressions below pin the boundary in both directions: the trailing raw
// key must be absorbed, and no keystroke that was not part of a commit gesture
// may ever be absorbed. What counts as the committing key is decided by the
// keystroke, never by the committed string: a commit with an empty string is
// the cancel path (Esc, or Backspace clearing a selection) and owes its raw
// key the same one-shot claim, and a commit with no composition behind it is
// unsolicited and owes nothing.
// ---------------------------------------------------------------------------

#[test]
fn fcitx_space_commit_leaves_no_trailing_space() {
    // The reported reproduction: pinyin "nihao" committed with `Space`.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    let mut preedit = String::new();
    for ch in ["n", "i", "h", "a", "o"] {
        preedit.push_str(ch);
        feed_typing_key(&mut rt, &preedit, ch);
    }
    assert_eq!(rt.pending_input_len(), 0, "raw Latin never reaches the PTY");

    feed_commit(&mut rt, "\u{4f60}\u{597d}", Some(char_key(" ")));

    let bytes = rt.drain_pending_input();
    assert_eq!(
        std::str::from_utf8(&bytes),
        Ok("\u{4f60}\u{597d}"),
        "commit inserts exactly the composed text"
    );
    assert!(
        !bytes.ends_with(b" "),
        "no synthetic trailing space after an fcitx commit (#1449)"
    );
}

#[test]
fn fcitx_enter_commit_does_not_submit_the_line() {
    // Same boundary, worse symptom: an `Enter` that committed the composition
    // must not also reach the shell as a bare carriage return.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    feed_typing_key(&mut rt, "ni", "i");
    feed_commit(
        &mut rt,
        "\u{4f60}",
        Some(KeyEvent {
            logical_key: LogicalKey::Named(NamedKey::Enter),
            text: Some("\r".to_string()),
            location: KeyLocation::Standard,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        }),
    );

    let bytes = rt.drain_pending_input();
    assert_eq!(std::str::from_utf8(&bytes), Ok("\u{4f60}"));
    assert!(
        !bytes.contains(&b'\r'),
        "the committing Enter must not submit the command line (#1449)"
    );
}

#[test]
fn commit_claim_is_bounded_to_one_press() {
    // The claim is one-shot: a swallowed key can never cascade into silently
    // dropping a second, real keystroke.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "n", "n");
    // Commit with the trailing raw key withheld, so the claim is still held
    // when the next press arrives.
    feed_commit(&mut rt, "\u{4f60}", None);
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}")
    );

    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "the owed commit key is absorbed"
    );
    assert!(
        rt.handle_key_event(char_key("b")).is_some(),
        "the press after the claim is real input"
    );
    assert!(
        rt.handle_key_event(char_key("c")).is_some(),
        "and so is every later press"
    );
    assert_eq!(std::str::from_utf8(&rt.drain_pending_input()), Ok("bc"));
}

#[test]
fn commit_echo_in_the_next_batch_is_still_absorbed() {
    // The batch-review P1. winit raises `AboutToWait` once per dispatch cycle
    // and the embedder ticks on it, but a compositor may flush the commit and
    // the echoing `wl_keyboard` key in two consecutive `wl_display`
    // roundtrips. Releasing the claim at the commit batch's tick therefore
    // handed the echo straight back to the terminal, and the fcitx5 trailing
    // space of #1449 returned on a perfectly ordinary compositor.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "ni", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);
    rt.handle_ime_commit_at("\u{4f60}\u{597d}".to_string(), base);
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}\u{597d}")
    );

    // Batch boundary for the commit arrives first; the echo lands after it.
    rt.tick_at(base);
    assert!(
        rt.handle_key_event(char_key(" ")).is_none(),
        "the echo is still the IME's one batch after the commit"
    );
    assert_eq!(
        rt.drain_pending_input(),
        b"",
        "and it inserts nothing (#1449)"
    );
}

#[test]
fn commit_echo_stays_absorbed_across_several_ticks() {
    // A busy event loop can dispatch several batches inside the echo window;
    // ageing must not accumulate into an early release.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "ni", "n");
    rt.handle_ime_preedit_at(Some("ni".to_string()), Some(2), base);
    rt.handle_ime_commit_at("\u{4f60}".to_string(), base);
    rt.drain_pending_input();

    // Nine ticks, still inside the window.
    for step in 1..=9 {
        rt.tick_at(base + IME_COMMIT_ECHO_WINDOW * step / 10);
    }
    assert!(
        rt.handle_key_event(char_key(" ")).is_none(),
        "ticks inside the window never drop the claim"
    );
}

#[test]
fn presented_and_skipped_frames_alike_do_not_release_the_claim_early() {
    // Ageing happens at the batch boundary, before the present gates, so it
    // does not matter whether a tick paints or the CTX-0380 synchronize-update
    // window skips it. Neither may release a claim that is still inside its
    // echo window.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "n", "n");
    rt.handle_ime_preedit_at(Some("n".to_string()), Some(1), base);
    rt.handle_ime_commit_at("\u{4f60}".to_string(), base);
    rt.drain_pending_input();

    // Ticks inside the window, some painting and some not; the claim must
    // survive all of them.
    let mut painted = 0usize;
    for step in 1..=8 {
        if rt
            .tick_at(base + IME_COMMIT_ECHO_WINDOW * step / 10)
            .is_some()
        {
            painted += 1;
        }
    }
    assert!(painted > 0, "at least one of these frames painted");
    assert!(
        rt.handle_key_event(char_key(" ")).is_none(),
        "no frame inside the window releases the claim"
    );
    assert_eq!(
        rt.drain_pending_input(),
        b"",
        "and the echo inserts nothing"
    );
}

#[test]
fn commit_claim_expires_once_the_echo_window_elapses() {
    // X11 XIM filters the committing key outright: winit emits the
    // preedit-clear and the commit and no `KeyboardInput` ever follows, so
    // nothing is owed. The claim must not outlive its window, or the user's
    // next keystroke would be swallowed on that backend.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "ni", "n");
    rt.handle_ime_preedit_at(Some("ni".to_string()), Some(2), base);
    rt.handle_ime_commit_at("\u{4f60}".to_string(), base);
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}")
    );

    rt.tick_at(base + IME_COMMIT_ECHO_WINDOW + std::time::Duration::from_millis(1));
    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "an expired claim never absorbs a keystroke"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn idle_after_commit_does_not_absorb_the_next_keystroke() {
    // The X11 shape with no tick in between, which a tick count could not
    // catch: the loop blocks after the commit and the user's next key is the
    // very next event to arrive. The deadline is what separates "the echo is
    // still in flight" from "the loop has been idle since the commit".
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "ni", "n");
    rt.handle_ime_preedit_at(
        Some("ni".to_string()),
        Some(2),
        base - IME_COMMIT_ECHO_WINDOW * 2,
    );
    rt.handle_ime_commit_at("\u{4f60}".to_string(), base - IME_COMMIT_ECHO_WINDOW * 2);
    rt.drain_pending_input();

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "a long-idle keystroke is never absorbed"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn commit_text_is_never_trimmed() {
    // Terminal Truth: the commit string is user input. Some input methods
    // deliberately append a space, and that space is not the #1449 defect —
    // trimming the commit would be a different, equally wrong "fix".
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    feed_commit(&mut rt, "\u{4f60} ", None);
    rt.tick();

    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60} "),
        "a trailing space inside the commit survives verbatim"
    );
}

#[test]
fn empty_commit_owns_its_own_key_and_nothing_more() {
    // An empty commit inserts no bytes, but the keystroke that produced it is
    // still a physical key whose raw copy reaches `wl_keyboard`, so the claim
    // survives it: exactly one press is absorbed (the echo), and the press
    // after that is real input again. The one-shot accounting is the guarantee
    // here; `empty_commit_after_a_live_composition_absorbs_its_own_raw_key`
    // pins the reported ordering itself.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    feed_commit(&mut rt, "", None);
    assert_eq!(
        rt.pending_input_len(),
        0,
        "an empty commit inserts no bytes at all"
    );

    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "the owed commit key is absorbed"
    );
    assert!(
        rt.handle_key_event(char_key("b")).is_some(),
        "the press after the claim is real input, so the claim stays one-shot"
    );
    assert_eq!(rt.pending_input(), b"b");
}

#[test]
fn empty_commit_after_a_live_composition_absorbs_its_own_raw_key() {
    // The reported ordering for an empty commit: a live composition, then
    // `Preedit("")` and `Commit("")` in the same `done`, then the raw copy of
    // the key that closed it. Releasing the claim on the empty string handed
    // that key straight back to the PTY — the #1449 trailing space, on the
    // cancel path, which is the same defect with an empty commit instead of a
    // full one.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    let mut preedit = String::new();
    for ch in ["n", "i", "h", "a", "o"] {
        preedit.push_str(ch);
        feed_typing_key(&mut rt, &preedit, ch);
    }
    feed_commit(&mut rt, "", Some(char_key(" ")));

    let bytes = rt.drain_pending_input();
    assert_eq!(
        bytes, b"",
        "an empty commit plus its own key insert nothing"
    );
    assert!(
        !bytes.ends_with(b" "),
        "no synthetic trailing space after an empty commit (#1449)"
    );
}

#[test]
fn empty_commit_without_a_preedit_clear_still_absorbs_its_own_raw_key() {
    // The same empty commit arriving with no `Preedit("")` in front of it, so
    // the claim is still `Composing` when the commit lands: a backend that
    // delivers `done` without the preedit clear must not reopen the keyboard
    // either, and the claim must still end up deadline-bounded rather than
    // staying `Composing` (which would never hand the terminal back a key).
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "nihao", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);
    rt.handle_ime_commit_at(String::new(), base);
    let _ = rt.handle_platform_event(key_event(char_key(" ")));
    rt.tick_at(base);

    assert_eq!(rt.drain_pending_input(), b"", "nothing reaches the PTY");
    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "and the keyboard is live again, so the claim was bounded"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn empty_commit_claim_expires_once_the_echo_window_elapses() {
    // The X11 XIM shape on the cancel path: the key is filtered server-side, so
    // no echo ever arrives. The claim must age out rather than eat the first
    // keystroke of the next word — an empty commit is not a licence to hold
    // the keyboard, it is the same bounded claim as any other commit.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "nihao", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);
    rt.handle_ime_preedit_at(None, None, base);
    rt.handle_ime_commit_at(String::new(), base);

    rt.tick_at(base + IME_COMMIT_ECHO_WINDOW + std::time::Duration::from_millis(1));
    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "an expired claim never absorbs a keystroke"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn empty_commit_restamps_the_claim_it_keeps() {
    // Which piece of the `done` the window is measured from is a real choice,
    // so it is pinned: the commit is the last IME evidence of the gesture, and
    // an input method that clears the preedit and commits late must not have
    // its claim expire in the gap between the two. Stamping at the preedit
    // clear alone would release at the tick below and hand the echo back.
    //
    // The tick is the only clock that moves: the key path reads the real clock,
    // which is still well inside every window here, so the assertion is about
    // whether ageing dropped the claim at that tick.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "nihao", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);
    // Preedit-clear first, then the commit three quarters of a window later.
    rt.handle_ime_preedit_at(None, None, base);
    rt.handle_ime_commit_at(String::new(), base + IME_COMMIT_ECHO_WINDOW * 3 / 4);

    // Past the preedit-clear stamp, inside the commit's own stamp.
    rt.tick_at(base + IME_COMMIT_ECHO_WINDOW + std::time::Duration::from_millis(1));
    assert!(
        rt.handle_key_event(char_key(" ")).is_none(),
        "the commit re-stamped the claim, so the echo is still the IME's"
    );
    assert_eq!(rt.drain_pending_input(), b"");
}

#[test]
fn unsolicited_empty_commit_claims_no_keystroke() {
    // The other half of the boundary, and the reason the commit is not allowed
    // to claim unconditionally: a commit with no composition behind it (a
    // stray IME `done`, an input method committing without composing) owns no
    // key, because winit never announced a preedit for it. An *empty* one
    // especially so — with no text there is nothing to suggest a keystroke was
    // involved at all, and arming a claim here would silently swallow the
    // user's next key for the whole echo window.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    let _ = rt.handle_platform_event(ime_event(ImeEvent::Commit(String::new())));

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "an unsolicited empty commit must not eat the next keystroke"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn unsolicited_commit_claims_no_keystroke() {
    // A commit with no composition behind it (stray IME `done`, an input
    // method committing without composing) owns no key: winit never sent the
    // preedit, so no raw copy of a committing keystroke can be outstanding.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    rt.handle_ime_commit("\u{4f60}".to_string());

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "an unsolicited commit must not eat the next keystroke"
    );
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}a")
    );
}

#[test]
fn ime_disabled_releases_the_claim() {
    // A disabled input method will never deliver a raw committing key, so the
    // claim must not survive into the next keystroke.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    let _ = rt.handle_platform_event(ime_event(ImeEvent::Commit("\u{4f60}".to_string())));
    let _ = rt.handle_platform_event(ime_event(ImeEvent::Disabled));
    rt.drain_pending_input();

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "Disabled hands the keyboard straight back"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn focus_loss_during_composition_frees_the_keyboard() {
    // Issue #1356 cleared the stale preedit on focus loss so a lost clearing
    // event could not brick the keyboard forever. The claim has to go with it,
    // or a focus change mid-composition would strand the terminal the same way.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    assert!(rt.ime_preedit().is_some(), "composition is live");

    rt.set_focused(false);
    assert!(rt.ime_preedit().is_none(), "focus loss drops the preedit");
    rt.set_focused(true);

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "the keyboard is live again after a focus change"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn modifier_chords_are_never_absorbed_by_the_claim() {
    // Modifier state must stay in sync with the platform, so a bare modifier
    // press is never consumed — and it must not spend the claim either, since
    // the committing key's raw copy still has to be absorbed.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    feed_commit(&mut rt, "\u{4f60}", None);
    rt.drain_pending_input();

    assert!(
        rt.handle_key_event(KeyEvent {
            logical_key: LogicalKey::Named(NamedKey::Shift),
            text: None,
            location: KeyLocation::Left,
            state: PressState::Pressed,
            repeat: false,
            is_synthetic: false,
        })
        .is_none(),
        "a bare modifier press encodes nothing"
    );
    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "the modifier press did not spend the claim"
    );
    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "and the claim is still bounded to the one press"
    );
}

#[test]
fn non_ime_typing_is_untouched_across_frames() {
    // No IME event at all: every key is real input, spaces included, and a
    // frame between keys changes nothing.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    for ch in ["l", "s", " ", "-", "l", "a"] {
        rt.tick();
        assert!(
            rt.handle_key_event(char_key(ch)).is_some(),
            "non-IME key {ch:?} must reach the terminal"
        );
    }
    assert_eq!(std::str::from_utf8(&rt.drain_pending_input()), Ok("ls -la"));
}

#[test]
fn composition_overlay_and_caret_survive_the_claim() {
    // The claim is a key-path concern only: presentation state, the forwarded
    // caret rect, and grid truth are untouched by it.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let before = rt.snapshot();

    feed_typing_key(&mut rt, "ni", "n");
    feed_typing_key(&mut rt, "nih", "h");
    assert_eq!(rt.ime_preedit(), Some("nih"), "overlay tracks composition");

    // A frame lands mid-composition on every batch, and must not hand the
    // keyboard back to the terminal while the input method still owns it.
    assert!(rt.tick().is_some(), "preedit forces a present");
    assert!(
        rt.ime_cursor_area().is_some(),
        "caret rect stays armed for the OS candidate window"
    );
    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "a live composition keeps owning raw presses across a frame"
    );

    feed_commit(&mut rt, "\u{4f60}\u{597d}", Some(char_key(" ")));
    assert!(rt.ime_preedit().is_none(), "commit clears the overlay");
    assert!(rt.tick().is_some(), "commit still forces a present");
    let after = rt.snapshot();
    assert_eq!(before.cells, after.cells, "no grid mutation, claim or not");
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}\u{597d}")
    );
}

#[test]
fn repeated_commits_absorb_one_key_each() {
    // Two compositions in a row, each with its own trailing commit key: the
    // per-gesture accounting holds, so neither leaks and neither over-consumes.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "ni", "n");
    feed_commit(&mut rt, "\u{4f60}", Some(char_key(" ")));
    feed_typing_key(&mut rt, "hao", "h");
    feed_commit(&mut rt, "\u{597d}", Some(char_key(" ")));

    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}\u{597d}"),
        "neither commit leaves a trailing space"
    );
}

#[test]
fn a_repeat_press_never_spends_or_waits_on_the_commit_claim() {
    // A `repeat` press is the platform re-reporting a press it already
    // delivered, so it can never be the echo of the committing key. Holding
    // the commit key must therefore neither let the repeat eat the echo nor be
    // eaten by it: the repeat is real input and the claim stays armed for the
    // echo that follows.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "n", "n");
    rt.handle_ime_preedit_at(Some("n".to_string()), Some(1), base);
    rt.handle_ime_commit_at("\u{4f60}".to_string(), base);
    rt.drain_pending_input();

    let mut repeat_key = char_key(" ");
    repeat_key.repeat = true;
    assert!(
        rt.handle_key_event(repeat_key).is_some(),
        "a repeat press is real input, not the echo"
    );
    assert!(
        rt.handle_key_event(char_key(" ")).is_none(),
        "and the echo is still absorbed afterwards"
    );
    // Exactly the repeat's own space: the commit was drained above and the
    // echo contributed nothing.
    assert_eq!(std::str::from_utf8(&rt.drain_pending_input()), Ok(" "));
}

#[test]
fn a_repeat_press_is_still_absorbed_during_composition() {
    // The other half of the repeat boundary, unchanged from CTX-0367: while a
    // preedit is up the raw copy of *every* keystroke is suppressed, a held
    // key included, so composition still cannot double-input.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");

    feed_typing_key(&mut rt, "n", "n");
    let mut repeat_key = char_key("a");
    repeat_key.repeat = true;
    assert!(
        rt.handle_key_event(repeat_key).is_none(),
        "composition owns repeats too"
    );
    assert_eq!(rt.pending_input_len(), 0);
}

#[test]
fn a_live_composition_survives_unlimited_ticks() {
    // The guarantee the deadline must not touch: a live preedit owns the
    // keyboard for as long as it is up, however many batches pass in the
    // meantime. Ageing is a bound on the *post-commit* claim only, so ticks
    // spread over tens of echo windows — including instants far past any
    // deadline — still leave the composition in charge. Releasing on a tick is
    // the CTX-0367 double-input defect, and ageing must never grow into it.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "nihao", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);

    for step in 1..=32 {
        // Four windows per tick step, so the last tick is 128 windows in.
        rt.tick_at(base + IME_COMMIT_ECHO_WINDOW * 4 * step);
    }
    assert_eq!(rt.ime_preedit(), Some("nihao"), "the overlay is still up");

    assert!(
        rt.handle_key_event(char_key("a")).is_none(),
        "a live composition still owns raw presses after all those ticks"
    );
    let mut repeat_key = char_key("b");
    repeat_key.repeat = true;
    assert!(
        rt.handle_key_event(repeat_key).is_none(),
        "and still owns a held key, CTX-0367 unchanged"
    );
    assert_eq!(rt.pending_input_len(), 0, "nothing reaches the PTY");
}

#[test]
fn esc_cancel_absorbs_its_own_echo_then_hands_the_keyboard_back() {
    // A cancel is the same boundary with no commit behind it: winit sends the
    // empty preedit, and the raw `Esc` still arrives. It must be absorbed
    // exactly once, and the terminal must be live again right after.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "niha", "n");
    // Cancel: the preedit clears, no commit follows.
    rt.handle_ime_preedit_at(None, None, base);
    assert!(rt.ime_preedit().is_none(), "cancel clears the overlay");

    let esc = KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Escape),
        text: Some("\u{1b}".to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    };
    assert!(
        rt.handle_key_event(esc).is_none(),
        "the raw Esc of the cancel gesture is absorbed, not inserted"
    );
    assert_eq!(rt.pending_input_len(), 0, "a cancel inserts no bytes");

    assert!(
        rt.handle_key_event(char_key("a")).is_some(),
        "and the keyboard is live immediately after the cancel"
    );
    assert_eq!(rt.pending_input(), b"a");
}

#[test]
fn x11_xim_commit_batch_replays_preedit_then_commit_with_no_echo() {
    // The X11 XIM shape end to end, in one dispatched batch: winit's
    // `handle_xim_discard_event` pushes the preedit-clear and the commit for
    // the key XIM consumed, and the key itself never reaches the key path. The
    // claim is armed but unfilled, and it must age out rather than eat the
    // first keystroke of the next word.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    // Four letters, each XIM-consumed: only the preedits are visible.
    for index in 0..4 {
        let preedit: String = ["n", "i", "h", "a"][..=index].concat();
        rt.handle_ime_preedit_at(Some(preedit.clone()), Some(preedit.len()), base);
    }
    // The committing key: preedit-clear, then commit, no KeyboardInput.
    rt.handle_ime_preedit_at(None, None, base);
    rt.handle_ime_commit_at("\u{4f60}\u{597d}".to_string(), base);
    assert_eq!(
        std::str::from_utf8(&rt.drain_pending_input()),
        Ok("\u{4f60}\u{597d}")
    );

    // Idle, then the next word's first letter.
    rt.tick_at(base + IME_COMMIT_ECHO_WINDOW * 3 / 2);
    assert!(
        rt.handle_key_event(char_key("h")).is_some(),
        "an X11 XIM session never loses a keystroke to the claim"
    );
    assert_eq!(rt.pending_input(), b"h");
}

#[test]
fn wayland_commit_and_echo_in_one_batch_still_absorbs_the_echo() {
    // The compositor flushed both halves in a single `wl_display` roundtrip:
    // the commit and its `wl_keyboard` echo arrive before the batch's tick.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let base = std::time::Instant::now();

    feed_typing_key(&mut rt, "nihao", "n");
    rt.handle_ime_preedit_at(Some("nihao".to_string()), Some(5), base);
    rt.handle_ime_preedit_at(None, None, base);
    rt.handle_ime_commit_at("\u{4f60}\u{597d}".to_string(), base);
    let _ = rt.handle_platform_event(key_event(char_key(" ")));
    rt.tick_at(base);

    let bytes = rt.drain_pending_input();
    assert_eq!(std::str::from_utf8(&bytes), Ok("\u{4f60}\u{597d}"));
    assert!(!bytes.ends_with(b" "), "no trailing space (#1449)");
}
