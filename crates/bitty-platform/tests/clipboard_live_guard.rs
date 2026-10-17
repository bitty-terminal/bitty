//! Collector-level refusal regressions for PX-6898. No native backend calls.

#[path = "support/clipboard_isolation.rs"]
#[allow(dead_code)]
mod clipboard_isolation;

use clipboard_isolation::{BLOCKED, Observation, ProcessIdentity, collect, run_if_isolated};

fn observation() -> Observation {
    Observation {
        namespaces: Some(["mnt-new".into(), "net-new".into(), "pid-new".into()]),
        parent_namespaces: Some(["mnt-old".into(), "net-old".into(), "pid-old".into()]),
        runner: Some(ProcessIdentity {
            pid: 10,
            start: 100,
            parent: 1,
        }),
        server: Some(ProcessIdentity {
            pid: 11,
            start: 110,
            parent: 10,
        }),
        mounts: vec![
            "21 1 0:10 / /tmp rw - tmpfs tmpfs rw".into(),
            "22 1 0:11 / /run rw - tmpfs tmpfs rw".into(),
        ],
    }
}

fn assert_refused(before: Observation, after: Observation) {
    let mut observations = [before, after].into_iter();
    let evidence = collect(|| observations.next().expect("two collector snapshots"));
    assert!(observations.next().is_none());
    let mut native_calls = 0;
    assert_eq!(
        run_if_isolated(&evidence, || native_calls += 1),
        Err(BLOCKED)
    );
    assert_eq!(native_calls, 0);
}

#[test]
fn host_tmpfs_and_sibling_server_cannot_authorize_native_access() {
    let mut host = observation();
    host.namespaces = host.parent_namespaces.clone();
    assert_refused(host.clone(), host);
}

#[test]
fn reused_server_cannot_authorize_native_access() {
    let mut reused = observation();
    reused.server.as_mut().unwrap().start = 90;
    assert_refused(reused.clone(), reused);
}

#[test]
fn stale_or_replaced_server_evidence_never_invokes_native_closure() {
    let before = observation();
    let mut replaced = before.clone();
    replaced.server.as_mut().unwrap().start += 1;
    assert_refused(before.clone(), replaced);
    let mut stale = before.clone();
    stale.server = None;
    assert_refused(before, stale);
}

#[test]
fn replaced_mount_or_network_namespace_never_invokes_native_closure() {
    let before = observation();
    let mut replaced = before.clone();
    replaced.mounts[0] = "31 1 0:20 / /tmp rw - tmpfs tmpfs rw".into();
    assert_refused(before.clone(), replaced);
    let mut replaced = before.clone();
    replaced.namespaces.as_mut().unwrap()[1] = "net-replaced".into();
    assert_refused(before, replaced);
}

#[test]
fn even_stable_fresh_looking_evidence_cannot_bind_future_connection() {
    assert_refused(observation(), observation());
    assert_refused(Observation::default(), Observation::default());
}

#[test]
fn actual_collector_never_authorizes_native_operations() {
    let mut calls = 0;
    assert_eq!(
        run_if_isolated(&clipboard_isolation::current(), || calls += 1),
        Err(BLOCKED)
    );
    assert_eq!(calls, 0);
}

#[test]
fn actual_collector_refuses_forged_environment_hints_in_child_processes() {
    // Separate processes avoid modifying the parallel test harness environment.
    // None of these child tests constructs a native clipboard or opens a display.
    for pid in [
        "invalid".to_owned(),
        "0".into(),
        std::process::id().to_string(),
    ] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actual_collector_never_authorizes_native_operations",
            ])
            .env("BITTY_CLIPBOARD_XVFB_PID", pid)
            .env("DISPLAY", ":0")
            .output()
            .unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    }
}
