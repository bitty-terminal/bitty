//! `bitty --test-mode` end-to-end protocol proofs (CTX-0506, research 043).
//!
//! Drives the built `bitty` binary as an external E2E target: spawns the
//! binary with `--test-mode`, connects to the `BITTY_SOCKET` it serves, and
//! exercises the stable E2E surface over the existing `bitty.debug/*` framing
//! — no display, no GPU, no VM.
//!
//! Surface asserted here (flag-gated, default-deny without the flag):
//! - `bitty.debug/testInfo` handshake (test-mode only)
//! - live panel state: `listViews` / `listTerminals`
//! - panel control: `splitView` (fresh pane + shell), `focusView`, `sendInput`
//! - state assertions: `getTerminalText`, `getGridText` (grid + cursor)
//! - deterministic teardown: `testExit` (elevated `debug.control`)
//!
//! Headless and Unix-only: the servo is a same-UID `0600` Unix socket and
//! test mode runs the tick loop without a display server.

#![cfg(unix)]

mod live_instance;

use std::time::Duration;

use live_instance::{TestDir, TestInstance};

#[test]
fn test_mode_serves_stable_e2e_surface_end_to_end() {
    let mut instance = TestInstance::spawn_test_mode("e2e", true);
    let mut client = instance.connect();

    // 1. Flag-gated handshake: only a `--test-mode` instance answers.
    let info = client.call_ok("bitty.debug/testInfo", None);
    assert!(
        info.contains("\"test_mode\":true")
            && info.contains("\"surface\":\"e2e\"")
            && info.contains("\"protocol\":\"1.0\""),
        "testInfo must identify the E2E surface: {info}"
    );

    // 2. Live panel state: the primary leaf exists and owns the primary
    //    PTY session (the pane registry tracks split-pane sessions only, so
    //    `has_pane_session` is asserted on the split leaf in step 5).
    let views = client.call_ok("bitty.debug/listViews", None);
    assert!(views.contains("\"v:1\""), "primary view missing: {views}");
    let terminals = client.call_ok("bitty.debug/listTerminals", None);
    assert!(
        terminals.contains("\"id\":\"t:1\""),
        "primary terminal missing: {terminals}"
    );

    // 3. Screen + cursor assertion on the primary grid.
    let grid = client.call_ok("bitty.debug/getGridText", None);
    assert!(
        grid.contains("\"cursor\":{") && grid.contains("\"visible\":"),
        "getGridText must expose the cursor: {grid}"
    );

    // 4. Drive the primary pane: send keys, wait for the shell output.
    client.call_ok(
        "bitty.debug/sendInput",
        Some("{\"terminal_id\":\"t:1\",\"text\":\"echo BITTY_E2E_PRIMARY\\n\"}"),
    );
    client.wait_for(
        "bitty.debug/getTerminalText",
        Some("{\"terminal_id\":\"t:1\"}"),
        "BITTY_E2E_PRIMARY",
    );

    // 5. Panel creation: split the focused pane; the new leaf gets its own
    //    shell and focus follows it (CTX-0364/CTX-0387 parity).
    let split = client.call_ok("bitty.debug/splitView", Some("{\"direction\":\"right\"}"));
    assert!(
        split.contains("\"new_view\":\"v:2\""),
        "split must name the fresh view: {split}"
    );
    let terminals = client.call_ok("bitty.debug/listTerminals", None);
    assert!(
        terminals.contains("\"id\":\"t:2\"") && terminals.contains("\"has_pane_session\":true"),
        "split pane must own a live pane session: {terminals}"
    );

    // 6. Drive the fresh pane: send keys, wait for its independent output.
    client.call_ok(
        "bitty.debug/sendInput",
        Some("{\"terminal_id\":\"t:2\",\"text\":\"echo BITTY_E2E_SPLIT\\n\"}"),
    );
    client.wait_for(
        "bitty.debug/getTerminalText",
        Some("{\"terminal_id\":\"t:2\"}"),
        "BITTY_E2E_SPLIT",
    );

    // 7. Focus control stays observable.
    let focus = client.call_ok("bitty.debug/getFocus", None);
    assert!(
        focus.contains("\"focused_view\":2"),
        "split must move focus to the fresh view: {focus}"
    );

    // 8. Deterministic teardown through the existing control queue
    //    (`debug.control` is elevated explicitly; no ambient authority).
    let exit = client.call_ok("bitty.debug/testExit", None);
    assert!(exit.contains("\"exiting\":true"), "testExit: {exit}");
    let status = instance.wait_exit(Duration::from_secs(10));
    assert!(
        status.success(),
        "test-mode must exit 0 after testExit, got {status}; stderr:\n{}",
        instance.stderr_tail()
    );
}

#[test]
fn test_mode_exit_is_not_ambient_authority() {
    // Test mode registers `testExit`, but registration grants nothing: the
    // verb still needs the elevated `debug.control` scope, so a caller
    // without the explicit `BITTY_CTL_ELEVATE` allowlist is denied and the
    // instance keeps serving (no partial state, no shutdown).
    let mut instance = TestInstance::spawn_test_mode("noelev", false);
    let mut client = instance.connect();
    let denied = client.call("bitty.debug/testExit", None);
    assert!(
        denied.contains("\"error\"") && denied.contains("ScopeDenied"),
        "testExit without elevation must be ScopeDenied: {denied}"
    );
    let info = client.call_ok("bitty.debug/testInfo", None);
    assert!(
        info.contains("\"test_mode\":true"),
        "instance must keep serving after a denied testExit: {info}"
    );
}

#[test]
fn test_mode_unservable_socket_exits_nonzero_fail_closed() {
    // Amendment A3 admission: a test-mode process that cannot serve its
    // socket exits non-zero instead of running a harness against no
    // surface. `/proc` is never writable, so this socket can never bind
    // (fail-soft serve guard → `run()` returns 1).
    let dir = TestDir::new("nosock");
    let socket = "/proc/bitty-test-mode-unservable/s.sock".to_string();
    let mut instance = TestInstance::spawn_test_mode_with_socket("nosock", true, dir, socket);
    let status = instance.wait_exit(Duration::from_secs(30));
    assert!(
        !status.success(),
        "test-mode with an unservable socket must exit non-zero, got {status}"
    );
    assert!(
        instance.stderr_tail().contains("servable IPC socket"),
        "stderr must name the missing surface:\\n{}",
        instance.stderr_tail()
    );
}
