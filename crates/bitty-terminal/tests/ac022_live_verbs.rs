#![cfg(unix)]

//! `P0-AC-022` live terminal-instance cells (bitty#2921, CTX-1850).
//!
//! The dated `P0-AC-022` record
//! (`bitty-docs/docs/security/audits/bittyd-ac022-scopes-2026-11-16.md`) leaves
//! two live cells open on the terminal-instance surface, and this target is
//! those two cells. Each one drives the built `bitty` binary over its real
//! `0600` Unix socket with the shared [`live_instance`] fixture (the same
//! spawn, socket-wait, and raw-wire client `tests/test_mode.rs` uses), and
//! prints the observed typed result under an `AC022` marker.
//!
//! | Test | Verb | Arm | Tier |
//! | --- | --- | --- | --- |
//! | [`terminal_send_and_text_are_live_and_scope_bounded`] | `terminal send` / `terminal text` | allow + deny | live, headless `--test-mode` |
//! | [`window_new_denies_an_ended_session_on_a_served_surface`] | `ctl window new` | deny | live, headless `--test-mode` |
//! | [`window_new_creates_a_window_on_a_served_surface`] | `ctl window new` | allow + deny | live, windowed instance |
//! | [`window_new_denies_a_connection_without_view_manage`] | `ctl window new` | deny | unrun, `#[ignore]`d |
//!
//! # How each arm is produced
//!
//! A same-UID connection to the instance servo always negotiates the fixed CLI
//! default grant (`terminal.inspect`, `terminal.input`, `view.inspect`,
//! `view.manage`, `config.inspect`, `plugin.inspect`, plus the
//! `BITTY_CTL_ELEVATE` allowlist): the client never asserts scopes, so no
//! client can present a connection that was born without one of them. The deny
//! arms here therefore use the two server-owned reductions the servo itself
//! exposes:
//!
//! - `bitty.debug/revokeConsentScope` strips one consentable scope from the
//!   calling session and advances its consent generation, so the same
//!   connection is inspect-only afterwards and `bitty.debug/sendInput` is
//!   refused with `Denied/ScopeDenied` naming `terminal.input`;
//! - `bitty.debug/revokeConsentSession` ends the calling session, so any
//!   further request is refused with `Denied/Unauthenticated` before its
//!   handler runs — including the new-window lane.
//!
//! Both reductions are the production verbs (`crates/bitty-terminal/src/
//! consent.rs`); nothing here is a test-only scope shortcut.
//!
//! # What this target does not claim
//!
//! - It does not mark `P0-AC-022` verified, and it is not the independent
//!   security review the criterion's verification method names.
//! - The `view.manage`-absent arm for `window new` stays unrun: `view.manage`
//!   is outside the consent allowlist (`CONSENTABLE_SCOPES`), so no served
//!   connection can be driven into lacking it. That arm needs the arbitrary
//!   single-scope live grant of bitty#2917, and it is recorded as an
//!   `#[ignore]`d case rather than faked.
//! - `window new` is exercised on a windowed instance only; a host with no
//!   compositor socket reports the cell unrun with the reason on stdout. A
//!   headless runner is expected to print `AC022_UNRUN` there, which is not a
//!   pass.
//! - The served `bitty.debug/listWindows` answers a single-window stub
//!   (`crates/bitty-terminal/src/ctl/apply.rs`), so the created window's id is
//!   observed from the request's registry count and the app's own
//!   `window registered id=N` record, not from `window list`.
//!
//! Run with `cargo test -p bitty-terminal --test ac022_live_verbs --offline
//! -- --nocapture`.

mod live_instance;

use std::time::{Duration, Instant};

use live_instance::{POLL_INTERVAL, TestInstance};

/// Params naming the primary terminal.
const TEXT_T1: Option<&str> = Some(r#"{"terminal_id":"t:1"}"#);
/// Allowed input: the marker can only appear from the command's own output,
/// never from the echoed input line.
const SEND_ALLOWED: Option<&str> =
    Some(r#"{"terminal_id":"t:1","text":"printf 'AC022-%s\n' OUT\n"}"#);
/// Denied input, distinct marker so a silent apply is observable.
const SEND_DENIED: Option<&str> =
    Some(r#"{"terminal_id":"t:1","text":"printf 'AC022-%s\n' DENY\n"}"#);
/// The documented revoke verb, scoped to the input half only.
const REVOKE_INPUT: Option<&str> = Some(r#"{"scope":"terminal.input"}"#);

/// Assert the CLI/instance permission refusal for `scope` on `method`.
fn assert_scope_denied(response: &str, method: &str, scope: &str) {
    assert!(
        response.contains("\"error\":{"),
        "{method} must be refused: {response}"
    );
    assert!(
        response.contains("\"category\":\"scope\""),
        "{method} must refuse in the scope class: {response}"
    );
    assert!(
        response.contains("\"code\":\"ScopeDenied\""),
        "{method} must refuse with ScopeDenied: {response}"
    );
    assert!(
        response.contains(&format!("scope '{scope}'")),
        "{method} must name the missing scope {scope}: {response}"
    );
    assert!(
        response.contains(&format!("for {method}")),
        "{method} must name the verb it refused: {response}"
    );
    assert!(
        !response.contains("\"result\""),
        "a refusal must carry no result body: {response}"
    );
}

/// Poll the instance log for a line containing `needle`.
fn wait_for_log(instance: &TestInstance, needle: &str, budget: Duration) -> String {
    let start = Instant::now();
    loop {
        let log = instance.stderr_tail();
        if let Some(line) = log.lines().find(|line| line.contains(needle)) {
            return line.to_owned();
        }
        assert!(
            start.elapsed() < budget,
            "instance log never recorded {needle:?}: {log}"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `terminal text` (`terminal.inspect`) and `terminal send` (`terminal.input`)
/// over the socket, both arms.
///
/// The allow arm proves the input reaches the session: the grid grows the
/// command's output, which the echoed input line cannot produce. The deny arm
/// then strips `terminal.input` from this connection with the documented
/// revoke verb, asserts the typed `Denied/ScopeDenied` naming that scope, and
/// keeps polling the grid to prove the refused input changed nothing while the
/// read half still answers — reading terminal text implies no input authority.
#[test]
fn terminal_send_and_text_are_live_and_scope_bounded() {
    let mut instance = TestInstance::spawn_test_mode("ac022-send", false);
    let mut client = instance.connect();

    // Baseline: the read half works and the marker is not there yet.
    let before = client.call_ok("bitty.debug/getTerminalText", TEXT_T1);
    assert!(
        before.contains("\"terminal_id\":\"t:1\""),
        "terminal text must name the terminal it read: {before}"
    );
    assert!(
        !before.contains("AC022-OUT"),
        "fixture precondition: the marker must not be on screen yet: {before}"
    );
    println!("AC022 terminal.text allowed {before}");

    // Allow arm: the input verb answers with a receipt...
    let sent = client.call_ok("bitty.debug/sendInput", SEND_ALLOWED);
    assert!(
        sent.contains("\"sent_to\":\"t:1\""),
        "terminal send must report the terminal it drove: {sent}"
    );
    println!("AC022 terminal.send allowed {sent}");

    // ...and the session really changed: `AC022-OUT` is output, not echo.
    let effect = client.wait_for("bitty.debug/getTerminalText", TEXT_T1, "AC022-OUT");
    println!("AC022 terminal.send effect {effect}");

    // Deny arm: remove `terminal.input` from this connection's session.
    let revoked = client.call_ok("bitty.debug/revokeConsentScope", REVOKE_INPUT);
    assert!(
        revoked.contains("\"revoked\":true"),
        "the connection held terminal.input, so the revoke must report it: {revoked}"
    );
    println!("AC022 consent.revoke terminal.input {revoked}");

    let denied = client.call("bitty.debug/sendInput", SEND_DENIED);
    assert_scope_denied(&denied, "bitty.debug/sendInput", "terminal.input");
    println!("AC022 terminal.send denied {denied}");

    // No mutation: the refused input never reaches the grid, and the read half
    // (terminal.inspect) still answers on the same connection.
    let after = client.wait_absent(
        "bitty.debug/getTerminalText",
        TEXT_T1,
        "AC022-DENY",
        Duration::from_secs(1),
    );
    assert!(
        after.contains("AC022-OUT"),
        "the allowed input must stay on screen: {after}"
    );
    println!("AC022 terminal.text after denial {after}");

    // The revoke is idempotent and the denial is not a one-shot state.
    let again = client.call_ok("bitty.debug/revokeConsentScope", REVOKE_INPUT);
    assert!(
        again.contains("\"revoked\":false"),
        "a second revoke must report that nothing was left to revoke: {again}"
    );
    let denied_again = client.call("bitty.debug/sendInput", SEND_DENIED);
    assert_scope_denied(&denied_again, "bitty.debug/sendInput", "terminal.input");
    println!("AC022 terminal.send denied-again {denied_again}");
}

/// The deny arm of `ctl window new` that a headless host can still serve.
///
/// On a `--test-mode` loop the verb is registered and its `view.manage` gate is
/// passed, but no app thread drains the new-window lane, so the supervised
/// request reports `transport/Unavailable` and creates nothing — never a silent
/// success. Once the session is revoked the same verb is refused at the scope
/// stage with `Denied/Unauthenticated`, before the lane and before any window.
#[test]
fn window_new_denies_an_ended_session_on_a_served_surface() {
    let mut instance = TestInstance::spawn_test_mode("ac022-window-headless", false);
    let mut client = instance.connect();

    let unserviced = client.call("bitty.debug/newWindow", None);
    assert!(
        unserviced.contains("\"error\":{") && unserviced.contains("\"code\":\"Unavailable\""),
        "a headless loop must report the unserviced lane, not success: {unserviced}"
    );
    assert!(
        unserviced.contains("nothing was created"),
        "the unserviced reply must not claim a window: {unserviced}"
    );
    println!("AC022 window.new headless {unserviced}");

    let revoked = client.call_ok("bitty.debug/revokeConsentSession", None);
    assert!(
        revoked.contains("\"revoked\":true"),
        "the calling session must end: {revoked}"
    );
    println!("AC022 consent.revoke session {revoked}");

    // The last request this connection can carry: the servo ends it after the
    // revoke, so a further frame would be refused by the transport instead of
    // answering the scope question.
    let denied = client.call("bitty.debug/newWindow", None);
    assert!(
        denied.contains("\"category\":\"scope\"")
            && denied.contains("\"code\":\"Unauthenticated\""),
        "an ended session must be refused at the scope stage: {denied}"
    );
    assert!(
        denied.contains("connection authority is no longer active"),
        "the refusal must name the ended authority: {denied}"
    );
    println!("AC022 window.new denied {denied}");
}

/// `ctl window new` on a served surface, allow and deny arms.
///
/// The allow arm needs a real event loop: only the windowed app owns an
/// `EventContext`, and the headless loop reports the lane unserviced. The
/// instance is started on the host's compositor (discovered from the
/// environment, never hardcoded) with its own private root and socket, and the
/// app's own `window registered id=N` record plus the registry count in the
/// reply are what prove a window row exists — the served `window list` is a
/// single-window stub today, so it cannot enumerate the second window.
#[test]
fn window_new_creates_a_window_on_a_served_surface() {
    let Some(mut instance) = TestInstance::spawn_graphical("ac022-window") else {
        println!(
            "AC022_UNRUN window.new: this host exposes no compositor socket (WAYLAND_DISPLAY/DISPLAY \
             unusable), and the new-window lane is serviced only by the windowed app; nothing was proven"
        );
        return;
    };
    let mut client = instance.connect();

    let before = client.call_ok("bitty.debug/listWindows", None);
    assert!(
        before.contains("\"id\":\"w:1\""),
        "the primary window must be listed before the request: {before}"
    );
    println!("AC022 window.list before {before}");

    let created = client.call("bitty.debug/newWindow", None);
    if created.contains("\"code\":\"Unavailable\"")
        || created.contains("\"code\":\"WindowRefused\"")
    {
        println!(
            "AC022_UNRUN window.new: the served instance answered {created}; the lane was not \
             serviced on this host, so nothing was proven"
        );
        return;
    }
    assert!(
        created.contains("\"requested\":true"),
        "an admitted request must say it asked for a window: {created}"
    );
    assert!(
        created.contains("\"windows\":2"),
        "the registry must hold the primary window plus the created one: {created}"
    );
    println!("AC022 window.new allowed {created}");

    // The created row is a registry entry, not a counter: a second request
    // increments the same count, and the app records both ids it created.
    let second = client.call("bitty.debug/newWindow", None);
    assert!(
        second.contains("\"requested\":true") && second.contains("\"windows\":3"),
        "the second request must extend the same registry: {second}"
    );
    println!("AC022 window.new second {second}");

    let registered = wait_for_log(&instance, "window registered id=2", Duration::from_secs(5));
    println!("AC022 window.new registered {registered}");

    // Observation, not an assertion: the served list cannot enumerate the
    // second window (single-window stub), which is a finding for the issue.
    let listed = client.call_ok("bitty.debug/listWindows", None);
    println!("AC022 window.list after {listed}");

    // Deny arm: end the session, then ask again. The refusal comes from the
    // scope stage, before the lane, so no window can appear.
    let revoked = client.call_ok("bitty.debug/revokeConsentSession", None);
    assert!(
        revoked.contains("\"revoked\":true"),
        "the calling session must end: {revoked}"
    );
    let denied = client.call("bitty.debug/newWindow", None);
    assert!(
        denied.contains("\"category\":\"scope\"")
            && denied.contains("\"code\":\"Unauthenticated\""),
        "an ended session must be refused at the scope stage: {denied}"
    );
    println!("AC022 window.new denied {denied}");
}

/// Unrun arm: a connection that never held `view.manage`.
///
/// Every same-UID connection to the servo is granted the fixed CLI default,
/// `view.manage` included, and `view.manage` is deliberately outside the
/// consent allowlist (`CONSENTABLE_SCOPES` in `consent.rs`), so the served
/// surface cannot be driven into a `view.manage`-absent state. The handler-side
/// proof for exactly this refusal exists at the dispatcher tier
/// (`ipc_serve::tests::new_window_denies_a_connection_without_view_manage_before_the_lane`,
/// which builds a real `bitty_ipc::ctl::ControlAuthority` grant and a real
/// attested peer proof with `ScopeSet::mcp_default`), but a *live socket* cell
/// needs the arbitrary single-scope grant tracked as bitty#2917. Nothing here
/// fakes it.
#[test]
#[ignore = "unreachable live: the servo grants the fixed CLI default (view.manage included) to every same-uid connection and view.manage is outside the consent allowlist; the live arm needs bitty#2917's arbitrary single-scope grant"]
fn window_new_denies_a_connection_without_view_manage() {
    println!(
        "AC022_UNRUN window.new deny arm (view.manage absent): no served connection can lack \
         view.manage today; see bitty#2917"
    );
}
