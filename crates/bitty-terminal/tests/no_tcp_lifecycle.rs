//! No-TCP lifecycle cell (bitty#2921, CTX-1865).
//!
//! The security corpus records that no external no-TCP scan exists: the claim
//! that the daemon/instance holds no listening TCP socket and no outbound TCP
//! connection during a full session lifecycle has never been observed from
//! outside the process. This target is that observation on the live
//! terminal-instance surface.
//!
//! # No `AC-0xx` id is claimed
//!
//! The daemon corpus assigns the no-TCP control no acceptance-criterion id:
//! `crates/bitty-daemon/README.md:719` maps `P0-AC-023` to the *child scope
//! limitation*, which is a different control, and the no-TCP property appears
//! only as prose — "The daemon opens no TCP listener" at
//! `crates/bitty-daemon/README.md:89` — with no criterion attached. This target
//! therefore claims no AC id and its tags carry no AC number; which criterion
//! (if any) this observation rolls up into is the security reviewer's call.
//!
//! # Linux only
//!
//! The scan is an external `ss -tanp` query attributed through `/proc`, so the
//! whole target is `cfg(target_os = "linux")` — not `cfg(unix)`. macOS has
//! neither `ss` nor `/proc`, so a `unix` gate would admit a platform on which
//! the cell can only fail; on macOS and Windows this target now compiles to
//! nothing, and reports no pass it did not observe.
//!
//! | Phase | What the scan must show |
//! | --- | --- |
//! | startup | zero TCP rows for every PID in the instance's process group |
//! | attached | zero TCP rows *while* a client is connected (the attach is the `0600` Unix socket) |
//! | after detach | zero TCP rows once the client is gone |
//! | stop (`testExit`) | every PID the fixture could still see is gone, so none of them holds a socket |
//! | abort (`SIGKILL`) | the recorded PID is gone and no visible member of its tree holds a TCP row |
//!
//! [`live_instance::tcp_absence::self_check`] runs first and fails the cell if
//! `ss` cannot attribute a real loopback listener to a known PID, so "zero
//! rows" is never a vacuous result. A host without `ss` fails with the reason
//! (see `live_instance/tcp_absence.rs`): the criterion is an external scan, and
//! this target never reports a pass it did not observe.
//!
//! # What this target does not claim
//!
//! It is the terminal-instance half only. The `bittyd` daemon process, the
//! second-uid connection, and the full attach/detach/abort/drain live campaign
//! remain open on bitty#2921.
//!
//! The two post-mortem arms (after-stop, after-abort) are bounded by what
//! `/proc` can still attribute: [`TestInstance::session_pids`] walks the
//! recorded PID and its *descendants*, so a child that called `setsid()` and
//! outlived its parent is re-parented away and becomes invisible to those two
//! scans. They prove no *visible* process of the tree holds a socket after the
//! stop; they do not prove an escaped session leader left none behind, and the
//! phase table above is worded to match. Run it with
//! `cargo test -p bitty-terminal --test no_tcp_lifecycle --offline -- --nocapture`.
//!
//! # Post-mortem arms cannot see a `setsid`'d child
//!
//! Stated once more where it bites: after the recorded PID exits, a grandchild
//! that `setsid()`'d itself is no longer reachable from `/proc/<pid>/stat`
//! parent links, so `session_pids()` returns whatever is still attached and the
//! scan is silent about the escaped one. The startup/attached/after-detach arms
//! are unaffected (the tree still has its root). Closing that residual needs a
//! durable containment identity (the daemon's cgroup plan), which is out of
//! scope for this cell.

#![cfg(target_os = "linux")]

mod live_instance;

use std::time::Duration;

use live_instance::tcp_absence;
use live_instance::{POLL_INTERVAL, TestInstance};

/// The verb that proves the client is attached and being served.
const PING: &str = "bitty.debug/testInfo";

#[test]
fn live_instance_holds_no_tcp_sockets_across_its_lifecycle() {
    // A zero result only means something if the same scan can see a socket.
    let control = tcp_absence::self_check();
    println!(
        "NOTCP control: the scan attributed {control} LISTEN row(s) to this test process's \
         own loopback listener, so a zero instance result is not vacuous"
    );

    // ── startup ──────────────────────────────────────────────────────────
    let mut instance = TestInstance::spawn_test_mode("no-tcp", true);
    println!(
        "NOTCP instance pid={} socket={}",
        instance.pid(),
        instance.socket()
    );
    let startup = tcp_absence::scan(&instance);
    assert!(
        startup.pids.contains(&instance.pid()),
        "the process-group scan must include the instance PID {}: {:?}",
        instance.pid(),
        startup.pids
    );
    startup.assert_no_tcp("startup");

    // ── attached ─────────────────────────────────────────────────────────
    // Hold a live client across a scan: attach must use the Unix socket only.
    let mut client = instance.connect();
    let info = client.call_ok(PING, None);
    assert!(
        info.contains("\"test_mode\":true"),
        "the client must be served while attached: {info}"
    );
    let attached = tcp_absence::scan(&instance);
    assert!(
        attached.pids.contains(&instance.pid()),
        "the attached scan must still cover the instance PID: {:?}",
        attached.pids
    );
    attached.assert_no_tcp("attached");

    // ── after detach ─────────────────────────────────────────────────────
    drop(client);
    // Give the servo a tick to notice the closed connection, so the detach is
    // observable rather than assumed.
    std::thread::sleep(POLL_INTERVAL * 8);
    let detached = tcp_absence::scan(&instance);
    detached.assert_no_tcp("after-detach");

    // ── orderly stop ─────────────────────────────────────────────────────
    let mut client = instance.connect();
    let exit = client.call_ok("bitty.debug/testExit", None);
    assert!(exit.contains("\"exiting\":true"), "testExit: {exit}");
    let status = instance.wait_exit(Duration::from_secs(10));
    assert!(
        status.success(),
        "the instance must exit 0 after testExit, got {status}; stderr:\n{}",
        instance.stderr_tail()
    );
    drop(client);
    let pids_after_stop = instance.session_pids();
    assert!(
        pids_after_stop.is_empty(),
        "no process may outlive an orderly stop, found {pids_after_stop:?}"
    );
    tcp_absence::scan_pids(&pids_after_stop).assert_no_tcp("after-stop");
    println!(
        "NOTCP after-stop: no PID the fixture can still see remains, so none of them holds a TCP \
         socket (a `setsid`'d escapee is outside `/proc` attribution — see the header)"
    );

    // ── abort ────────────────────────────────────────────────────────────
    // An abrupt stop must not leave a socket behind either. The fixture
    // recorded this PID at spawn; nothing here matches a process by name.
    let mut aborted = TestInstance::spawn_test_mode("no-tcp-abort", false);
    let aborted_pid = aborted.pid();
    tcp_absence::scan(&aborted).assert_no_tcp("abort-arm-startup");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(aborted_pid).expect("a recorded PID fits in i32")),
        nix::sys::signal::Signal::SIGKILL,
    )
    .expect("SIGKILL the PID this fixture recorded");
    let status = aborted.wait_exit(Duration::from_secs(10));
    assert!(
        !status.success(),
        "an aborted instance must not report a clean exit, got {status}"
    );
    let pids_after_abort = aborted.session_pids();
    assert!(
        pids_after_abort.is_empty(),
        "no process may outlive the recorded PID being killed, found {pids_after_abort:?}"
    );
    tcp_absence::scan_pids(&pids_after_abort).assert_no_tcp("after-abort");
    println!(
        "NOTCP after-abort: pid {aborted_pid} was killed, no PID the fixture can still see remains, \
         and zero TCP rows were ever attributed to it"
    );
}
