//! CTX-0512: owned-process-tree kill over real process trees.
//!
//! Each probe spawns `/bin/sh` as a group leader that forks a long-lived
//! grandchild and reports its pid, so the tests prove the property the
//! mechanism exists for: killing the tree takes the grandchild too, never
//! just the direct child. POSIX-only: the Windows Job Object backend has
//! its own probes in `owned_tree_windows.rs`.

#![cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]

use std::io::{BufRead as _, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bitty_pty::{LeaderExit, OwnedTree, PtyBuilder, TreeBackend, TreeSignal};

/// Upper bound for any single wait in these probes.
const WAIT_BOUND: Duration = Duration::from_secs(10);

/// Pause between liveness polls.
const POLL: Duration = Duration::from_millis(5);

/// `sh` script: fork a sleeping grandchild, print its pid, then keep the
/// leader alive (or exit with the given code when `$1` is set).
const TREE_SCRIPT: &str = "sleep 60 & echo $!; if [ -n \"$1\" ]; then exit \"$1\"; fi; wait";

fn spawn_tree(exit_code: Option<i32>) -> (Child, OwnedTree, u32) {
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(TREE_SCRIPT).arg("sh");
    if let Some(code) = exit_code {
        command.arg(code.to_string());
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    OwnedTree::prepare_command(&mut command);
    let mut child = command.spawn().expect("spawn sh");
    // `prepare_command` pairs with `adopt_prepared` (the documented rule).
    let tree = OwnedTree::adopt_prepared(child.id()).expect("adopt the tree");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("grandchild pid line");
    let grandchild = line.trim().parse().expect("grandchild pid");
    (child, tree, grandchild)
}

/// Whether `pid` still runs. A zombie counts as gone: it already died, and
/// in a container without an init its reap never comes.
fn running(pid: u32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // The state follows the parenthesised command name.
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z" && state != "X"),
            Err(_) => false,
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
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
fn the_backend_kills_owned_trees_here() {
    assert!(TreeBackend::detect().kills_owned_tree());
}

#[test]
fn killing_the_tree_takes_the_grandchild_too() {
    let (mut child, tree, grandchild) = spawn_tree(None);
    assert!(running(grandchild), "grandchild starts alive");
    tree.signal(TreeSignal::Kill).expect("kill the group");
    assert_eq!(wait_leader_exit(&tree), LeaderExit::Signaled(9));
    wait_until("the grandchild to die", || !running(grandchild));
    let status = tree.retire(|| child.wait()).expect("reap");
    assert!(!status.success());
}

#[test]
fn leader_exit_is_observed_without_reaping() {
    let (mut child, tree, grandchild) = spawn_tree(Some(7));
    assert_eq!(wait_leader_exit(&tree), LeaderExit::Exited(7));
    // Observed twice: nothing reaped the leader, so its pid (the group id)
    // is still pinned and the surviving grandchild is still reachable.
    assert_eq!(wait_leader_exit(&tree), LeaderExit::Exited(7));
    assert!(running(grandchild), "the grandchild outlives its leader");
    tree.signal(TreeSignal::Kill)
        .expect("the pinned group still reaches the grandchild");
    wait_until("the grandchild to die", || !running(grandchild));
    let status = tree.retire(|| child.wait()).expect("reap");
    assert_eq!(status.code(), Some(7));
}

#[test]
fn a_retired_tree_refuses_every_signal() {
    let (mut child, tree, grandchild) = spawn_tree(None);
    tree.signal(TreeSignal::Kill).expect("kill");
    wait_until("the grandchild to die", || !running(grandchild));
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
fn signalling_a_drained_group_is_harmless() {
    let (mut child, tree, grandchild) = spawn_tree(Some(0));
    wait_leader_exit(&tree);
    tree.signal(TreeSignal::Kill).expect("kill the leftover");
    wait_until("the grandchild to die", || !running(grandchild));
    // Only the zombie leader remains: a signal finds no live member on some
    // kernels and reports success on others; it never reaches anyone else.
    match tree.signal(TreeSignal::Kill) {
        Ok(()) => {}
        Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
    }
    let _ = tree.retire(|| child.wait());
}

#[test]
fn this_process_group_is_never_a_target() {
    let (mut child, tree, _grandchild) = spawn_tree(None);
    let own = own_process_group();
    let error = tree
        .signal_group(own, TreeSignal::Kill)
        .expect_err("own group refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    for refused in [0, 1] {
        let error = tree
            .signal_group(refused, TreeSignal::Kill)
            .expect_err("reserved group refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    tree.signal(TreeSignal::Kill).expect("kill");
    let _ = tree.retire(|| child.wait());
}

#[test]
fn graceful_signals_reach_the_whole_group() {
    let (mut child, tree, grandchild) = spawn_tree(None);
    tree.signal(TreeSignal::Terminate)
        .expect("terminate the group");
    assert_eq!(wait_leader_exit(&tree), LeaderExit::Signaled(15));
    wait_until("the grandchild to stop", || !running(grandchild));
    let _ = tree.retire(|| child.wait());
}

#[test]
fn a_pty_child_leads_an_owned_tree() {
    bitty_test_support::require_pty!();
    let mut pty = PtyBuilder::new("/bin/sh")
        .args(["-c".to_owned(), TREE_SCRIPT.to_owned(), "sh".to_owned()])
        .spawn()
        .expect("spawn pty sh");
    let leader = pty.pid().expect("pty child pid");
    let tree = OwnedTree::adopt(leader).expect("a session leader leads its group");
    tree.signal(TreeSignal::Kill).expect("kill the group");
    assert_eq!(wait_leader_exit(&tree), LeaderExit::Signaled(9));
    let status = tree.retire(|| pty.wait()).expect("reap");
    assert!(!status.is_success());
}

/// This test process's own group (`/proc` on Linux, `ps` on macOS), so the
/// probe needs no extra dependency.
fn own_process_group() -> u32 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").expect("own stat");
        // After the parenthesised name: state, ppid, pgrp.
        let (_, rest) = stat.rsplit_once(')').expect("stat shape");
        rest.split_whitespace()
            .nth(2)
            .and_then(|pgrp| pgrp.parse().ok())
            .expect("own pgrp")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let output = Command::new("ps")
            .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("own pgid")
    }
}
