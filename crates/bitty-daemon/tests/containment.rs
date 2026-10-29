#![cfg(target_os = "linux")]

//! Live containment evidence (bitty#2791).
//!
//! These tests use the real probe on the host and spawn real children. They do
//! not assume cgroup delegation: when the cgroup plan is unavailable the
//! subreaper fallback runs the same assertions and must report `unverified`
//! cleanup. A host with neither mechanism records a typed skip instead of a
//! pass.

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bitty_daemon::containment::{
    CleanupOutcome, ContainmentError, ContainmentKind, ContainmentLimits, ContainmentRecord,
    ContainmentRefusal, ContainmentReport, spawn,
};
use bitty_daemon::orphan::{
    BootId, OrphanJournal, OrphanJournalLimits, OrphanSupervisor, ProcessProbe, SystemProbe,
};
use bitty_daemon::platform::PlatformCell;
use bitty_daemon::registry::{CleanupClass, MonotonicMs, SessionId};

static NEXT: AtomicU32 = AtomicU32::new(0);

fn report() -> Option<ContainmentReport> {
    let report = ContainmentReport::probe();
    if !report.is_enforced() {
        eprintln!("containment probe refused: {report}");
        return None;
    }
    eprintln!("containment probe: {report}");
    Some(report)
}

fn name() -> String {
    format!(
        "bittyd-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // An exited, not-yet-reaped process is a zombie. It can no longer run and
    // an adopted child's zombie belongs to the subreaper, not to this test.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    !matches!(rest.trim_start().chars().next(), Some('Z' | 'X' | 'x'))
}

/// Wait until the pid has no `/proc` entry at all.
///
/// Unlike [`alive`], this does not accept a zombie: the direct child is reaped
/// by its owning handle, so a leftover `Z` would mean the reap ordering is
/// wrong.
fn wait_reaped(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn limits() -> ContainmentLimits {
    ContainmentLimits::new(Duration::from_millis(150), Duration::from_secs(2))
}

fn wait_gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn assert_cleanup(report: &ContainmentReport, outcome: CleanupOutcome) {
    let expected = match report.cleanup_claim() {
        "swept" => CleanupClass::Swept,
        _ => CleanupClass::Unverified,
    };
    assert_eq!(outcome.class(), expected, "{report}");
}

#[test]
fn a_child_that_ignores_sigterm_is_contained_and_killed() {
    let Some(report) = report() else {
        return;
    };
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("trap '' TERM; while :; do sleep 0.05; done");
    let mut child = spawn(&report, &name(), &mut command, limits()).expect("contained spawn");
    let pid = child.identity().pid().get();
    assert!(alive(pid));
    let outcome = child.terminate(limits());
    assert_cleanup(&report, outcome);
    assert!(wait_reaped(pid), "the SIGTERM-ignoring child survived");
}

#[test]
fn a_forked_grandchild_is_contained() {
    let Some(report) = report() else {
        return;
    };
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 30 & echo $!; wait")
        .stdout(Stdio::piped());
    let mut child = spawn(&report, &name(), &mut command, limits()).expect("contained spawn");
    let pid = child.identity().pid().get();
    let stdout = child.take_stdout().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("grandchild pid is numeric");
    assert!(alive(grandchild));

    let outcome = child.terminate(limits());
    assert_cleanup(&report, outcome);
    assert!(wait_reaped(pid), "the parent survived containment");
    assert!(wait_gone(grandchild), "the forked grandchild escaped");
}

#[test]
fn a_recorded_decoy_is_never_signalled() {
    let Some(report) = report() else {
        return;
    };
    let mut decoy = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("decoy spawn");
    let decoy_pid = decoy.id();
    let mut command = Command::new("sh");
    command.arg("-c").arg("sleep 30");
    let mut child = spawn(&report, &name(), &mut command, limits()).expect("contained spawn");
    let outcome = child.terminate(limits());
    assert_cleanup(&report, outcome);
    assert!(
        alive(decoy_pid),
        "a process outside the recorded identity was signalled"
    );

    // Cleanup uses the pid this test recorded at launch, never a name match.
    let _ = decoy.kill();
    let _ = decoy.wait();
}

#[test]
fn the_subreaper_fallback_kills_the_recorded_tree_and_never_claims_a_sweep() {
    let report = ContainmentReport::subreaper(
        PlatformCell::current(),
        Some("forced by the test".to_owned()),
    );
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("sleep 30 & echo $!; wait")
        .stdout(Stdio::piped());
    let mut child = match spawn(&report, &name(), &mut command, limits()) {
        Ok(child) => child,
        Err(ContainmentError::Subreaper(_)) => {
            eprintln!("subreaper unavailable in this environment; typed skip");
            return;
        }
        Err(error) => panic!("fallback spawn failed: {error}"),
    };
    let pid = child.identity().pid().get();
    let stdout = child.take_stdout().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("grandchild pid is numeric");

    let outcome = child.terminate(limits());
    assert_eq!(outcome.class(), CleanupClass::Unverified);
    assert!(wait_reaped(pid));
    assert!(wait_gone(grandchild));
}

#[test]
fn the_subreaper_fallback_kills_a_descendant_that_ignores_sigterm() {
    // The leader exits on SIGTERM while a descendant ignores it. The leader is
    // kept unreaped through the grace window so the recorded process group
    // cannot be recycled, and the escalation still reaches the descendant.
    let report = ContainmentReport::subreaper(
        PlatformCell::current(),
        Some("forced by the test".to_owned()),
    );
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("trap 'exit 0' TERM; (trap '' TERM; exec sleep 30) & echo $!; wait")
        .stdout(Stdio::piped());
    let mut child = match spawn(&report, &name(), &mut command, limits()) {
        Ok(child) => child,
        Err(ContainmentError::Subreaper(_)) => {
            eprintln!("subreaper unavailable in this environment; typed skip");
            return;
        }
        Err(error) => panic!("fallback spawn failed: {error}"),
    };
    let pid = child.identity().pid().get();
    let stdout = child.take_stdout().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("descendant pid");
    let descendant: u32 = line.trim().parse().expect("descendant pid is numeric");
    assert!(alive(descendant));

    let outcome = child.terminate(limits());
    assert_eq!(outcome.class(), CleanupClass::Unverified);
    assert!(wait_reaped(pid), "the leader was not reaped");
    assert!(
        wait_gone(descendant),
        "the SIGTERM-ignoring descendant escaped the recorded group"
    );
}

#[test]
fn a_double_forked_setsid_child_with_closed_stdio_is_contained() {
    let Some(report) = report() else {
        return;
    };
    if report.kind() != ContainmentKind::CgroupV2 {
        eprintln!("cgroup plan unavailable; typed skip");
        return;
    }
    if Command::new("setsid").arg("--version").output().is_err() {
        eprintln!("setsid unavailable; typed skip");
        return;
    }
    // `setsid` runs as a process-group leader (the spawn sets the group), so it
    // forks and exits: the contained process is a double-forked session leader
    // with every standard descriptor closed.
    let mut command = Command::new("setsid");
    command
        .arg("sh")
        .arg("-c")
        .arg("sleep 30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = spawn(&report, &name(), &mut command, limits()).expect("contained spawn");
    let pid = child.identity().pid().get();
    let Some(path) = child.containment().cgroup_path().map(ToOwned::to_owned) else {
        panic!("a cgroup record carries its subtree path");
    };
    let members = std::fs::read_to_string(path.join("cgroup.procs")).expect("cgroup.procs");
    assert!(
        !members.trim().is_empty(),
        "the detached session leader left the containment identity"
    );

    let outcome = child.terminate(limits());
    assert_eq!(
        outcome.class(),
        CleanupClass::Swept,
        "a setsid descendant cannot leave the cgroup identity"
    );
    assert!(wait_reaped(pid));
    assert!(!path.exists(), "the verified subtree was not removed");
}

#[test]
fn a_reserved_record_from_a_crash_is_reaped_through_its_subtree() {
    // The crash-injection analogue for C-13: a reserved record with no
    // committed pid whose subtree already holds a live member is still
    // actionable through the pre-created cgroup.
    let report = ContainmentReport::probe();
    let Some(root) = report.cgroup_root() else {
        eprintln!("cgroup unavailable; typed skip");
        return;
    };
    let name = name();
    let cgroup_path = root.join(&name);
    std::fs::create_dir(&cgroup_path).expect("session cgroup");
    let mut parent = Command::new("sh")
        .arg("-c")
        .arg("sleep 30 & echo $!; exit 0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("orphan pid");
    let orphan: u32 = line.trim().parse().expect("orphan pid");
    let _ = parent.wait();
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(cgroup_path.join("cgroup.procs"), format!("{orphan}\n"))
        .expect("join orphan to cgroup");

    let directory = tempfile::tempdir().expect("temp dir");
    let journal_dir = directory.path().join("orphans");
    let session = SessionId::parse(&"ac".repeat(16)).expect("session");
    {
        let mut journal = OrphanJournal::open(
            journal_dir,
            OrphanJournalLimits::default(),
            BootId::read_system().expect("boot id"),
        )
        .expect("journal");
        // Reserved only: the crash happened between spawn and commit.
        journal
            .reserve(
                session,
                0,
                MonotonicMs::from_millis(1),
                ContainmentRecord::cgroup(&cgroup_path),
            )
            .expect("reserve");
    }
    let mut supervisor = OrphanSupervisor::open(
        directory.path().join("orphans"),
        &OrphanJournalLimits::default(),
        &report,
        limits(),
    )
    .expect("supervisor");
    let verified = supervisor
        .verify(MonotonicMs::from_millis(2))
        .expect("verify")
        .expect("served");
    assert_eq!(verified.reaped, 1);
    assert_eq!(verified.unverified, 0);
    assert!(!alive(orphan), "the reserved subtree survived");
    assert!(
        !cgroup_path.exists(),
        "the verified subtree was not removed"
    );
}

#[test]
fn a_refused_plan_spawns_nothing() {
    let report = ContainmentReport::refused(
        PlatformCell::current(),
        ContainmentRefusal::UnsupportedPlatform {
            cell: PlatformCell::current(),
        },
    );
    let mut command = Command::new("sleep");
    command.arg("30");
    let error = spawn(&report, &name(), &mut command, limits()).expect_err("refused");
    assert!(error.to_string().contains("containment refused"));
}

#[test]
fn a_real_recorded_cgroup_orphan_is_reaped_and_verified() {
    let report = ContainmentReport::probe();
    let Some(root) = report.cgroup_root() else {
        eprintln!("cgroup unavailable; typed skip");
        return;
    };
    let name = name();
    let cgroup_path = root.join(&name);
    std::fs::create_dir(&cgroup_path).expect("session cgroup");
    // A detached grandchild with no handle in this process: the test is not a
    // subreaper here, so init reaps it after the cgroup kill.
    let mut parent = Command::new("sh")
        .arg("-c")
        .arg("sleep 30 & echo $!; exit 0")
        .stdout(Stdio::piped())
        .spawn()
        .expect("parent");
    let mut line = String::new();
    BufReader::new(parent.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("orphan pid");
    let orphan: u32 = line.trim().parse().expect("orphan pid");
    let _ = parent.wait();
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(cgroup_path.join("cgroup.procs"), format!("{orphan}\n"))
        .expect("join orphan to cgroup");
    let probe = SystemProbe::new();
    let bitty_daemon::orphan::ProcessObservation::Present {
        start_time,
        process_group,
        uid,
    } = probe.observe(bitty_daemon::orphan::Pid::new(orphan).expect("pid"))
    else {
        panic!("orphan must be observable");
    };
    let identity = bitty_daemon::orphan::ProcessIdentity::new(
        bitty_daemon::orphan::Pid::new(orphan).expect("pid"),
        start_time,
        process_group,
        uid,
    );

    let directory = tempfile::tempdir().expect("temp dir");
    let journal_dir = directory.path().join("orphans");
    let session = SessionId::parse(&"ab".repeat(16)).expect("session");
    {
        let mut journal = OrphanJournal::open(
            journal_dir,
            OrphanJournalLimits::default(),
            BootId::read_system().expect("boot id"),
        )
        .expect("journal");
        journal
            .reserve(
                session,
                0,
                MonotonicMs::from_millis(1),
                ContainmentRecord::cgroup(&cgroup_path),
            )
            .expect("reserve");
        journal.commit(session, identity).expect("commit");
    }
    let mut supervisor = OrphanSupervisor::open(
        directory.path().join("orphans"),
        &OrphanJournalLimits::default(),
        &report,
        ContainmentLimits::default(),
    )
    .expect("supervisor");
    let verified = supervisor
        .verify(MonotonicMs::from_millis(2))
        .expect("verify")
        .expect("served");
    assert_eq!(verified.reaped, 1);
    assert_eq!(verified.unverified, 0);
    assert!(!alive(orphan), "the recorded orphan survived");
    assert!(
        !cgroup_path.exists(),
        "the verified subtree was not removed"
    );
}
