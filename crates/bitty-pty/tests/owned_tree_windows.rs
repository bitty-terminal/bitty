//! CTX-0903: Windows Job Object owned-tree backend over real process trees.
//!
//! The leader is this test binary re-executed with [`HELPER_ENV`] set: it
//! starts a sleeping grandchild (the same binary again), prints
//! `grandchild=<pid>;`, and then sleeps or exits. The probes prove the
//! property the backend exists for: a tree kill takes the grandchild too,
//! never only the direct child. Liveness is checked through the safe
//! `bitty-winjob` probe, so the test needs no `unsafe`.
//!
//! Every wait is bounded by a named constant; nothing blocks unbounded.

#![cfg(windows)]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bitty_pty::{LeaderExit, OwnedTree, PtyBuilder, TreeBackend, TreeSignal};
use bitty_test_support::require_pty;

const HELPER_ENV: &str = "__BITTY_OWNED_TREE_WINDOWS_HELPER";

/// Test name the helper re-executes into.
const HELPER_TEST: &str = "__bitty_owned_tree_windows_helper_entry__";

/// Upper bound for any single wait in these probes.
const WAIT_BOUND: Duration = Duration::from_secs(20);

/// Pause between polls.
const POLL: Duration = Duration::from_millis(10);

/// How long helpers stay alive: far past every probe deadline.
const HELPER_LIFETIME: Duration = Duration::from_secs(60);

/// Exit code of the `fork-exit7` helper mode.
const LEADER_EXIT_CODE: i32 = 7;

/// Exit code the Job Object backend kills with (`Child::kill`'s code).
const TREE_KILL_EXIT_CODE: i32 = 1;

/// Child entry point: selected by `HELPER_ENV`, a no-op in the parent suite.
#[test]
fn __bitty_owned_tree_windows_helper_entry__() {
    match std::env::var(HELPER_ENV).as_deref() {
        Ok("sleep") => std::thread::sleep(HELPER_LIFETIME),
        Ok("fork") => {
            spawn_grandchild();
            std::thread::sleep(HELPER_LIFETIME);
        }
        Ok("fork-exit7") => {
            spawn_grandchild();
            std::process::exit(LEADER_EXIT_CODE);
        }
        // `quiet` and the parent suite: exit at once.
        _ => {}
    }
}

/// Starts a sleeping grandchild and announces its pid on stdout.
// Never waited on by design: the grandchild must outlive this helper so the
// probes can prove the tree kill reaches it.
#[allow(clippy::zombie_processes)]
fn spawn_grandchild() {
    use std::io::Write as _;
    let grandchild = helper_command("sleep")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("grandchild");
    println!("grandchild={};", grandchild.id());
    let _ = std::io::stdout().flush();
}

fn helper_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .args([HELPER_TEST, "--exact", "--nocapture"])
        .env(HELPER_ENV, mode);
    command
}

/// Spawns a prepared helper leader and adopts its tree.
fn spawn_tree(mode: &str) -> (Child, OwnedTree) {
    let mut command = helper_command(mode);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    OwnedTree::prepare_command(&mut command);
    let child = command.spawn().expect("spawn helper");
    let tree = OwnedTree::adopt_prepared(child.id()).expect("adopt the prepared tree");
    (child, tree)
}

/// Reads the `grandchild=<pid>;` announcement within [`WAIT_BOUND`]. The
/// reader thread is detached: it ends at EOF once the tree is gone.
fn read_grandchild(child: &mut Child) -> u32 {
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let pid = line
                .split_once("grandchild=")
                .and_then(|(_, rest)| rest.split_once(';'))
                .and_then(|(digits, _)| digits.parse::<u32>().ok());
            if let Some(pid) = pid {
                let _ = sender.send(pid);
                return;
            }
        }
    });
    receiver
        .recv_timeout(WAIT_BOUND)
        .expect("grandchild pid announced")
}

/// Whether the grandchild `pid` still runs.
///
/// Pid-reuse caveat: nothing pins a grandchild's pid in this process, so
/// once it dies Windows may recycle the pid. A recycled pid owned by
/// another user or a protected process answers `PermissionDenied`, which
/// therefore means "not our process": gone. A recycled pid we can open
/// would read as running and fail the probe loudly (never pass silently);
/// the bounded waits keep that window tiny.
fn running(pid: u32) -> bool {
    match bitty_winjob::process_is_running(pid) {
        Ok(running) => running,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => false,
        Err(error) => panic!("liveness probe for {pid} failed: {error}"),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT_BOUND;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(POLL);
    }
}

fn wait_leader_exit(tree: &OwnedTree) -> LeaderExit {
    let deadline = Instant::now() + WAIT_BOUND;
    loop {
        if let Some(exit) = tree.leader_exit().expect("observe leader") {
            return exit;
        }
        assert!(Instant::now() < deadline, "leader never exited");
        std::thread::sleep(POLL);
    }
}

#[test]
fn the_backend_is_a_job_object() {
    assert_eq!(TreeBackend::detect(), TreeBackend::JobObject);
    assert!(TreeBackend::detect().kills_owned_tree());
}

#[test]
fn killing_the_tree_takes_the_grandchild_too() {
    let (mut child, tree) = spawn_tree("fork");
    assert_eq!(tree.backend(), TreeBackend::JobObject);
    let grandchild = read_grandchild(&mut child);
    assert!(running(grandchild), "grandchild starts alive");
    tree.signal(TreeSignal::Kill).expect("terminate the job");
    assert_eq!(
        wait_leader_exit(&tree),
        LeaderExit::Exited(TREE_KILL_EXIT_CODE)
    );
    wait_until("the grandchild to die", || !running(grandchild));
    let status = tree.retire(|| child.wait()).expect("reap");
    assert_eq!(status.code(), Some(TREE_KILL_EXIT_CODE));
}

#[test]
fn leader_exit_is_observed_without_reaping() {
    let (mut child, tree) = spawn_tree("fork-exit7");
    let grandchild = read_grandchild(&mut child);
    assert_eq!(
        wait_leader_exit(&tree),
        LeaderExit::Exited(LEADER_EXIT_CODE)
    );
    // Observed twice: nothing was consumed.
    assert_eq!(
        wait_leader_exit(&tree),
        LeaderExit::Exited(LEADER_EXIT_CODE)
    );
    assert!(running(grandchild), "the grandchild outlives its leader");
    tree.signal(TreeSignal::Kill)
        .expect("the job still reaches the grandchild");
    wait_until("the grandchild to die", || !running(grandchild));
    let status = tree.retire(|| child.wait()).expect("reap");
    assert_eq!(status.code(), Some(LEADER_EXIT_CODE));
}

#[test]
fn graceful_signals_are_typed_unsupported_and_reach_nobody() {
    let (mut child, tree) = spawn_tree("fork");
    let grandchild = read_grandchild(&mut child);
    for signal in [TreeSignal::Interrupt, TreeSignal::Terminate] {
        let error = tree.signal(signal).expect_err("kill-only backend");
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
    // Windows has no process groups: no other "group" can be targeted.
    let error = tree
        .signal_group(tree.leader(), TreeSignal::Kill)
        .expect_err("no process groups");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    for refused in [0, 1] {
        let error = tree
            .signal_group(refused, TreeSignal::Kill)
            .expect_err("reserved group refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    // Nothing above fell back to a single-pid kill.
    assert_eq!(tree.leader_exit().expect("observe"), None);
    assert!(running(grandchild));
    tree.signal(TreeSignal::Kill).expect("kill");
    wait_until("the grandchild to die", || !running(grandchild));
    let _ = tree.retire(|| child.wait());
}

#[test]
fn a_drained_job_reports_no_member() {
    let (mut child, tree) = spawn_tree("quiet");
    wait_leader_exit(&tree);
    // The job's accounting drops the exited leader; until it does, a kill
    // of the (already exited) members is harmless.
    wait_until("the empty job to report no member", || {
        matches!(
            tree.signal(TreeSignal::Kill),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    });
    let status = tree.retire(|| child.wait()).expect("reap");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn a_retired_tree_refuses_every_signal() {
    let (mut child, tree) = spawn_tree("sleep");
    tree.signal(TreeSignal::Kill).expect("kill");
    let _ = tree.retire(|| child.wait());
    assert!(tree.is_retired());
    for signal in [
        TreeSignal::Interrupt,
        TreeSignal::Terminate,
        TreeSignal::Kill,
    ] {
        let error = tree.signal(signal).expect_err("retired");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
    let error = tree.leader_exit().expect_err("retired");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn dropping_the_tree_kills_what_is_left() {
    let (mut child, tree) = spawn_tree("fork");
    let grandchild = read_grandchild(&mut child);
    drop(tree);
    wait_until("kill-on-close to end the grandchild", || {
        !running(grandchild)
    });
    let _ = child.wait();
}

#[test]
fn a_conpty_child_is_adopted_after_the_spawn() {
    require_pty!();
    let mut pty = PtyBuilder::new("cmd.exe").spawn().expect("spawn cmd");
    let leader = pty.pid().expect("conpty child pid");
    let tree = OwnedTree::adopt(leader).expect("a running child joins its job");
    assert_eq!(tree.backend(), TreeBackend::JobObject);
    tree.signal(TreeSignal::Kill).expect("terminate the job");
    assert_eq!(
        wait_leader_exit(&tree),
        LeaderExit::Exited(TREE_KILL_EXIT_CODE)
    );
    let status = tree
        .retire(|| pty.wait_timeout(WAIT_BOUND))
        .expect("reap")
        .expect("the killed child exits in time");
    assert!(!status.is_success());
}
