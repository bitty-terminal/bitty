//! bitty#2601 / CTX-1619: public orphan-journal behavior.
//!
//! The fake probe and recording reaper implement the exported traits, so these
//! tests prove the signalling seam is usable without spawning a process and
//! that unverified records never reach it.

use bitty_daemon::orphan::{
    BootId, OrphanCounts, OrphanJournal, OrphanJournalLimits, OrphanReaper, Pid, ProcessGroupId,
    ProcessIdentity, ProcessObservation, ProcessProbe, ProcessStartTime, ReapResult,
};
use bitty_daemon::registry::{MonotonicMs, SessionId};
use tempfile::TempDir;

const BOOT: &str = "33333333-3333-4333-8333-333333333333";

fn boot() -> BootId {
    BootId::parse(BOOT).expect("canonical boot id")
}

fn session() -> SessionId {
    SessionId::parse(&"11".repeat(16)).expect("canonical session id")
}

fn identity() -> ProcessIdentity {
    ProcessIdentity::new(
        Pid::new(4321).expect("positive pid"),
        ProcessStartTime::new(55_000),
        ProcessGroupId::new(4321).expect("positive group"),
    )
}

struct FakeProbe {
    boot_id: Option<BootId>,
    observation: ProcessObservation,
}

impl ProcessProbe for FakeProbe {
    fn boot_id(&self) -> Option<BootId> {
        self.boot_id.clone()
    }

    fn observe(&self, _pid: Pid) -> ProcessObservation {
        self.observation
    }
}

struct RecordingReaper {
    calls: Vec<ProcessIdentity>,
    result: ReapResult,
}

impl OrphanReaper for RecordingReaper {
    fn reap(&mut self, orphan: &bitty_daemon::orphan::VerifiedOrphan) -> ReapResult {
        self.calls.push(orphan.identity());
        self.result
    }
}

fn journal(directory: &TempDir) -> OrphanJournal {
    OrphanJournal::open(
        directory.path().join("journal"),
        OrphanJournalLimits::default(),
        boot(),
    )
    .expect("journal opens")
}

#[test]
fn exact_identity_is_the_only_path_to_the_reaper() {
    let directory = TempDir::new().expect("temp dir");
    let mut journal = journal(&directory);
    let session_id = session();
    journal
        .reserve(session_id, 0, MonotonicMs::from_millis(1))
        .expect("reserve");
    journal.commit(session_id, identity()).expect("commit");
    assert_eq!(journal.counts().committed, 1);

    let probe = FakeProbe {
        boot_id: Some(boot()),
        observation: ProcessObservation::Present {
            start_time: ProcessStartTime::new(55_000),
            process_group: ProcessGroupId::new(4321).expect("positive"),
        },
    };
    let mut reaper = RecordingReaper {
        calls: Vec::new(),
        result: ReapResult::Swept,
    };
    let report = journal
        .reap(&probe, &mut reaper, MonotonicMs::from_millis(2))
        .expect("reap");

    assert_eq!(report.reaped, 1);
    assert_eq!(report.unverified, 0);
    assert_eq!(reaper.calls, vec![identity()]);
    assert_eq!(journal.counts().total(), 0);
}

#[test]
fn missing_pid_and_reused_pid_never_reach_the_reaper() {
    let directory = TempDir::new().expect("temp dir");
    let mut journal = journal(&directory);
    let reserved = session();
    let reused = SessionId::parse(&"22".repeat(16)).expect("canonical session id");

    journal
        .reserve(reserved, 0, MonotonicMs::from_millis(1))
        .expect("reserve");
    journal
        .reserve(reused, 1, MonotonicMs::from_millis(1))
        .expect("reserve");
    journal.commit(reused, identity()).expect("commit");

    let probe = FakeProbe {
        boot_id: Some(boot()),
        observation: ProcessObservation::Present {
            start_time: ProcessStartTime::new(55_001),
            process_group: ProcessGroupId::new(4321).expect("positive"),
        },
    };
    let mut reaper = RecordingReaper {
        calls: Vec::new(),
        result: ReapResult::Swept,
    };
    let report = journal
        .reap(&probe, &mut reaper, MonotonicMs::from_millis(2))
        .expect("reap");

    assert_eq!(
        report,
        bitty_daemon::orphan::ReapReport {
            reaped: 0,
            unverified: 2,
            stale: 0
        }
    );
    assert!(reaper.calls.is_empty(), "unverified records were signalled");
    assert_eq!(
        journal.counts(),
        OrphanCounts {
            reserved: 0,
            committed: 0,
            unverified: 2
        }
    );
}

#[cfg(unix)]
#[test]
fn the_journal_directory_and_record_are_owner_private() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    let directory = TempDir::new().expect("temp dir");
    let mut journal = journal(&directory);
    journal
        .reserve(session(), 0, MonotonicMs::from_millis(1))
        .expect("reserve");

    let journal_dir = directory.path().join("journal");
    assert_eq!(
        fs::metadata(&journal_dir)
            .expect("directory metadata")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let record = fs::read_dir(&journal_dir)
        .expect("journal entries")
        .find_map(|entry| {
            let path = entry.expect("entry").path();
            (path.extension().and_then(|extension| extension.to_str()) == Some("orphan"))
                .then_some(path)
        })
        .expect("one orphan record");
    assert_eq!(
        fs::metadata(record)
            .expect("record metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn the_record_contains_only_the_fenced_identity() {
    let directory = TempDir::new().expect("temp dir");
    let mut journal = journal(&directory);
    let session_id = session();
    journal
        .reserve(session_id, 0, MonotonicMs::from_millis(1))
        .expect("reserve");
    journal.commit(session_id, identity()).expect("commit");

    let text = std::fs::read_to_string(
        directory
            .path()
            .join("journal")
            .join(format!("{session_id}.orphan")),
    )
    .expect("record file");
    for key in [
        "session_id",
        "generation",
        "class",
        "boot_id",
        "reserved_at_ms",
        "validated_at_ms",
        "pid",
        "start_time",
        "process_group",
    ] {
        assert!(text.contains(&format!("{key} ")), "missing {key}");
    }
    for forbidden in ["argv", "env", "cwd", "content", "spawn"] {
        assert!(!text.contains(forbidden), "{forbidden} leaked");
    }
}
