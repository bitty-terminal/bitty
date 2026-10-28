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

use bitty_daemon::containment::ContainmentRecord;
use bitty_daemon::containment::{
    CleanupOutcome, ContainmentLimits, ContainmentRefusal, ContainmentReport, spawn,
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
    let mut child = spawn(&report, &name(), &mut command).expect("contained spawn");
    let pid = child.identity().pid().get();
    assert!(alive(pid));
    let outcome = child.terminate(ContainmentLimits::new(
        Duration::from_millis(150),
        Duration::from_secs(2),
    ));
    assert_cleanup(&report, outcome);
    assert!(wait_gone(pid), "the SIGTERM-ignoring child survived");
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
    let mut child = spawn(&report, &name(), &mut command).expect("contained spawn");
    let pid = child.identity().pid().get();
    let stdout = child.take_stdout().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("grandchild pid is numeric");
    assert!(alive(grandchild));

    let outcome = child.terminate(ContainmentLimits::new(
        Duration::from_millis(150),
        Duration::from_secs(2),
    ));
    assert_cleanup(&report, outcome);
    assert!(wait_gone(pid), "the parent survived containment");
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
    let mut child = spawn(&report, &name(), &mut command).expect("contained spawn");
    let outcome = child.terminate(ContainmentLimits::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
    ));
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
    let mut child = spawn(&report, &name(), &mut command).expect("fallback spawn");
    let pid = child.identity().pid().get();
    let stdout = child.take_stdout().expect("piped stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("grandchild pid");
    let grandchild: u32 = line.trim().parse().expect("grandchild pid is numeric");

    let outcome = child.terminate(ContainmentLimits::new(
        Duration::from_millis(150),
        Duration::from_secs(2),
    ));
    assert_eq!(outcome.class(), CleanupClass::Unverified);
    assert!(wait_gone(pid));
    assert!(wait_gone(grandchild));
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
    let error = spawn(&report, &name(), &mut command).expect_err("refused");
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
