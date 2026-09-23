//! Shell-input regression for issue #1356: `exit` hangs, Ctrl+C dead.
//!
//! Pins the live-PTY contract through the real input path, aligned with
//! ghostty/kitty behavior:
//!
//! - typed `exit` + Enter reaches the shell and the primary child exit is
//!   observable via [`Runtime::primary_exit_status`] (the embedder closes
//!   the session on it instead of freezing);
//! - `Ctrl+C` delivered as a key press (tracked control state, no `text`
//!   payload — the Wayland shape) arrives as `0x03` and the kernel line
//!   discipline turns it into SIGINT in the foreground child;
//! - `Ctrl+D` arrives as `0x04` and a line-oriented child observes EOF;
//! - a stale IME preedit never bricks keyboard input across focus loss.
//!
//! The byte-delivery tests need a live PTY (Unix + `require_pty!`); the
//! preedit unit test runs everywhere.

#![forbid(unsafe_code)]

use bitty_platform::{
    KeyEvent, KeyLocation, LogicalKey, ModifiersState, NamedKey, PlatformEvent, PressState,
    WindowEventKind, WindowId,
};
use bitty_runtime::{Runtime, ViewId};
use std::time::{Duration, Instant};

/// Live-shell wait budget (mirrors `m1_shell_coverage::SHELL_TIMEOUT`).
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);

fn char_key(ch: &str, text: Option<&str>) -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Character(ch.to_string()),
        text: text.map(|s| s.to_string()),
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

fn enter_key() -> KeyEvent {
    KeyEvent {
        logical_key: LogicalKey::Named(NamedKey::Enter),
        text: None,
        location: KeyLocation::Standard,
        state: PressState::Pressed,
        repeat: false,
        is_synthetic: false,
    }
}

/// Drives one platform event through the runtime, like the embedder.
fn drive(rt: &mut Runtime, kind: WindowEventKind) {
    let event = PlatformEvent::Window {
        window_id: WindowId::from_raw_public(1),
        kind,
    };
    let _ = rt.handle_platform_event(event);
}

/// Presses `logical` with tracked control held (the Wayland `Ctrl+letter`
/// shape: `ModifiersChanged` first, then the press with `text=None`).
fn ctrl_press(rt: &mut Runtime, logical: &str) {
    drive(
        rt,
        WindowEventKind::ModifiersChanged(ModifiersState {
            shift: false,
            control: true,
            alt: false,
            super_pressed: false,
        }),
    );
    drive(rt, WindowEventKind::KeyboardInput(char_key(logical, None)));
    drive(
        rt,
        WindowEventKind::ModifiersChanged(ModifiersState {
            shift: false,
            control: false,
            alt: false,
            super_pressed: false,
        }),
    );
}

fn grid_text(rt: &Runtime) -> String {
    rt.snapshot().cells.iter().map(|cell| cell.glyph).collect()
}

/// Polls until `done` or the budget elapses (primary pump).
fn wait_primary(rt: &mut Runtime, done: &dyn Fn(&Runtime) -> bool) -> bool {
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        if done(rt) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = rt.poll_pty_timeout(Duration::from_millis(50));
    }
}

/// Polls until the primary child exits, returning its status.
fn wait_primary_exit(rt: &mut Runtime) -> Option<bitty_pty::ExitStatus> {
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        if let Some(status) = rt.primary_exit_status() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        let _ = rt.poll_pty_timeout(Duration::from_millis(50));
    }
}

/// Polls until the pane child exits, returning its status.
fn wait_pane_exit(rt: &mut Runtime, view: &ViewId) -> Option<bitty_pty::ExitStatus> {
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        if let Some(status) = rt.pane_try_wait(view) {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        let _ = rt.poll_pty_timeout(Duration::from_millis(50));
    }
}

#[test]
fn primary_exit_status_is_none_without_a_child() {
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    assert!(
        rt.primary_exit_status().is_none(),
        "no child owned must never report an exit"
    );
}

#[test]
#[cfg(unix)]
fn typed_exit_reaches_primary_shell_and_reports_its_exit() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.spawn_shell_with_args("/bin/sh", &[])
        .expect("primary shell must attach");
    assert!(
        wait_primary(&mut rt, &|rt| {
            let grid = grid_text(rt);
            grid.contains('$') || grid.contains('#')
        }),
        "no shell prompt"
    );
    // Type `exit` through the key path plus Enter, exactly like production.
    for ch in ["e", "x", "i", "t"] {
        let text = ch.to_string();
        rt.handle_key_event(char_key(ch, Some(&text)));
    }
    rt.handle_key_event(enter_key());
    let status = wait_primary_exit(&mut rt);
    let Some(status) = status else {
        panic!(
            "shell never exited after typed exit+Enter; grid={:?}",
            grid_text(&rt)
        );
    };
    assert!(status.is_success(), "clean exit expected, got {status:?}");
    // The status reaps exactly once: a second poll reports nothing.
    assert!(
        rt.primary_exit_status().is_none(),
        "consumed exit status must not repeat"
    );
}

#[test]
#[cfg(unix)]
fn ctrl_c_key_path_delivers_sigint_to_the_foreground_child() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let view = rt.focused_view().expect("focused leaf");
    // `exec` replaces the shell with `sleep` so the observed status is the
    // interrupted child itself.
    rt.spawn_shell_for_view(view, "/bin/sh", &["-c", "exec sleep 30"], 80, 24)
        .expect("pane shell must attach");
    std::thread::sleep(Duration::from_secs(1));
    ctrl_press(&mut rt, "c");
    let status = wait_pane_exit(&mut rt, &view);
    let Some(status) = status else {
        panic!("foreground sleep survived Ctrl+C");
    };
    assert!(
        !status.is_success(),
        "signal death is not success, got {status:?}"
    );
    assert!(
        status.signal().is_some(),
        "foreground child must die by signal (SIGINT via line discipline), got {status:?}"
    );
}

#[test]
#[cfg(unix)]
fn ctrl_c_key_path_interrupts_and_the_shell_survives() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.spawn_shell_with_args("/bin/sh", &[])
        .expect("primary shell must attach");
    assert!(
        wait_primary(&mut rt, &|rt| {
            let grid = grid_text(rt);
            grid.contains('$') || grid.contains('#')
        }),
        "no shell prompt"
    );
    rt.write_input(b"sleep 30\r");
    std::thread::sleep(Duration::from_secs(1));
    ctrl_press(&mut rt, "c");
    // A SIGINT-interrupted sleep returns the shell to its prompt, so the
    // next command runs: the session survives Ctrl+C (ghostty/kitty parity).
    rt.write_input(b"echo AFTER1356\r");
    assert!(
        wait_primary(&mut rt, &|rt| grid_text(rt).contains("AFTER1356")),
        "shell never recovered after Ctrl+C; grid={:?}",
        grid_text(&rt)
    );
}

#[test]
#[cfg(unix)]
fn ctrl_d_key_path_delivers_eof_to_a_line_oriented_child() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    let view = rt.focused_view().expect("focused leaf");
    rt.spawn_shell_for_view(view, "/bin/cat", &[], 80, 24)
        .expect("pane cat must attach");
    std::thread::sleep(Duration::from_secs(1));
    ctrl_press(&mut rt, "d");
    let status = wait_pane_exit(&mut rt, &view);
    let Some(status) = status else {
        panic!("cat survived Ctrl+D (EOF never delivered)");
    };
    assert!(
        status.is_success(),
        "cat exits cleanly on EOF, got {status:?}"
    );
}

#[test]
#[cfg(unix)]
fn exit_eof_on_an_empty_shell_line_closes_the_session() {
    bitty_test_support::require_pty!();
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.spawn_shell_with_args("/bin/sh", &[])
        .expect("primary shell must attach");
    assert!(
        wait_primary(&mut rt, &|rt| {
            let grid = grid_text(rt);
            grid.contains('$') || grid.contains('#')
        }),
        "no shell prompt"
    );
    // Ctrl+D on an empty cooked line is EOF: the shell exits.
    ctrl_press(&mut rt, "d");
    assert!(
        wait_primary_exit(&mut rt).is_some(),
        "shell never exited after Ctrl+D on an empty line; grid={:?}",
        grid_text(&rt)
    );
}

#[test]
fn focus_loss_clears_a_stale_ime_preedit_and_unblocks_keys() {
    // Issue #1356: a preedit whose clearing event never arrives (IME
    // restart, backend quirk) must not brick keyboard input forever while
    // output stays fine. Losing input focus drops the stale composition —
    // a composition cannot outlive focus — and typing flows again.
    let mut rt = Runtime::with_defaults().expect("headless runtime must build");
    rt.set_focused(true);
    rt.handle_ime_preedit(Some(String::from("ni")), None);
    assert_eq!(rt.ime_preedit(), Some("ni"));
    assert!(
        rt.handle_key_event(char_key("x", Some("x"))).is_none(),
        "raw presses stay consumed while a composition is active"
    );
    rt.drain_pending_input();
    rt.set_focused(false);
    assert_eq!(rt.ime_preedit(), None, "focus loss clears the preedit");
    rt.drain_pending_input();
    assert_eq!(
        rt.handle_key_event(char_key("x", Some("x"))),
        Some(b"x".to_vec()),
        "typing flows again after the stale preedit clears"
    );
    assert_eq!(rt.drain_pending_input(), b"x");
}
